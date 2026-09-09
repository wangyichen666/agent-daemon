use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::warn;

use crate::provider::ToolSpec;
use crate::safety::SafetyPolicy;
use crate::tools::{DynamicToolSource, Tool};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct ServerConfig {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct McpStatus {
    pub file_error: Option<String>,
    pub servers: Vec<McpServerStatus>,
}

#[derive(Clone, Debug)]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

#[derive(Default)]
struct McpSnapshot {
    status: McpStatus,
    tools: HashMap<String, Arc<dyn Tool>>,
}

pub struct McpManager {
    workspace: PathBuf,
    safety: Arc<SafetyPolicy>,
    snapshot: std::sync::RwLock<McpSnapshot>,
    clients: Mutex<Vec<Arc<McpClient>>>,
    reload_lock: Mutex<()>,
}

impl McpManager {
    pub fn new(workspace: &Path, safety: Arc<SafetyPolicy>) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            safety,
            snapshot: std::sync::RwLock::new(McpSnapshot::default()),
            clients: Mutex::new(Vec::new()),
            reload_lock: Mutex::new(()),
        }
    }

    pub async fn reload(&self) {
        let _guard = self.reload_lock.lock().await;
        let parsed = load_config(&self.workspace).await;
        let mut status = McpStatus::default();
        let mut next_clients = Vec::new();
        let mut next_tools = HashMap::<String, Arc<dyn Tool>>::new();

        match parsed {
            ConfigLoad::Disabled => {}
            ConfigLoad::FileError(error) => status.file_error = Some(error),
            ConfigLoad::Servers { valid, invalid } => {
                status.servers.extend(invalid);
                for config in valid {
                    match McpClient::connect(&config).await {
                        Ok((client, remote_tools)) => {
                            let mut names = Vec::new();
                            let mut collision = None;
                            for remote in remote_tools {
                                let bridge_name = bridge_name(&config.name, &remote.name);
                                if next_tools.contains_key(&bridge_name) {
                                    collision = Some(format!("MCP 工具名冲突：{bridge_name}"));
                                    break;
                                }
                                names.push(bridge_name.clone());
                                next_tools.insert(
                                    bridge_name.clone(),
                                    Arc::new(McpTool {
                                        bridge_name,
                                        remote_name: remote.name,
                                        description: remote.description,
                                        input_schema: remote.input_schema,
                                        server_name: config.name.clone(),
                                        client: client.clone(),
                                        safety: self.safety.clone(),
                                    }),
                                );
                            }
                            if let Some(error) = collision {
                                next_tools.retain(|name, _| !names.contains(name));
                                client.shutdown().await;
                                status.servers.push(McpServerStatus {
                                    name: config.name,
                                    connected: false,
                                    tools: Vec::new(),
                                    error: Some(error),
                                });
                            } else {
                                status.servers.push(McpServerStatus {
                                    name: config.name,
                                    connected: true,
                                    tools: names,
                                    error: None,
                                });
                                next_clients.push(client);
                            }
                        }
                        Err(error) => status.servers.push(McpServerStatus {
                            name: config.name,
                            connected: false,
                            tools: Vec::new(),
                            error: Some(format!("{error:#}")),
                        }),
                    }
                }
            }
        }

        status
            .servers
            .sort_by(|left, right| left.name.cmp(&right.name));
        {
            let mut snapshot = self
                .snapshot
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *snapshot = McpSnapshot {
                status,
                tools: next_tools,
            };
        }
        let previous = {
            let mut clients = self.clients.lock().await;
            std::mem::replace(&mut *clients, next_clients)
        };
        for client in previous {
            client.shutdown().await;
        }
    }

    pub fn status(&self) -> McpStatus {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone()
    }

    pub async fn shutdown(&self) {
        let clients = {
            let mut clients = self.clients.lock().await;
            std::mem::take(&mut *clients)
        };
        for client in clients {
            client.shutdown().await;
        }
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.tools.clear();
        for server in &mut snapshot.status.servers {
            server.connected = false;
        }
    }
}

impl DynamicToolSource for McpManager {
    fn specs(&self) -> Vec<ToolSpec> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters(),
            })
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools
            .get(name)
            .cloned()
    }
}

struct McpTool {
    bridge_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    server_name: String,
    client: Arc<McpClient>,
    safety: Arc<SafetyPolicy>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.bridge_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        self.safety
            .authorize_external_action(
                &format!("{} / {}", self.server_name, self.remote_name),
                &arguments,
            )
            .await?;
        self.client.call_tool(&self.remote_name, arguments).await
    }
}

#[derive(Clone, Debug)]
struct RemoteTool {
    name: String,
    description: String,
    input_schema: Value,
}

struct McpClient {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl McpClient {
    async fn connect(config: &ServerConfig) -> Result<(Arc<Self>, Vec<RemoteTool>)> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("启动 MCP server {} 失败", config.name))?;
        let stdin = child.stdin.take().context("MCP server 未提供 stdin")?;
        let stdout = child.stdout.take().context("MCP server 未提供 stdout")?;
        let stderr = child.stderr.take().context("MCP server 未提供 stderr")?;
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            reader_task: Mutex::new(None),
            stderr_task: Mutex::new(None),
        });
        let reader_client = Arc::downgrade(&client);
        *client.reader_task.lock().await = Some(tokio::spawn(async move {
            let result = reader_loop(stdout, reader_client.clone()).await;
            if let Some(client) = reader_client.upgrade() {
                client
                    .fail_pending(result.err().map_or_else(
                        || "MCP server stdout 已关闭".to_owned(),
                        |error| format!("MCP stdio 读取失败：{error:#}"),
                    ))
                    .await;
            }
        }));
        *client.stderr_task.lock().await = Some(tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match stderr.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => warn!(message = %line.trim_end(), "MCP server stderr"),
                    Err(error) => {
                        warn!(%error, "读取 MCP server stderr 失败");
                        break;
                    }
                }
            }
        }));

        let handshake = async {
            let initialized = client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "my-agent", "version": env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            initialized
                .get("protocolVersion")
                .and_then(Value::as_str)
                .context("MCP initialize 响应缺少 protocolVersion")?;
            client
                .notify("notifications/initialized", json!({}))
                .await?;
            let listed = client.request("tools/list", json!({})).await?;
            parse_remote_tools(&listed)
        }
        .await;
        match handshake {
            Ok(tools) => Ok((client, tools)),
            Err(error) => {
                client.shutdown().await;
                Err(error)
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .write_message(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        tokio::time::timeout(MCP_REQUEST_TIMEOUT, receiver)
            .await
            .with_context(|| format!("MCP 请求超时：{method}"))?
            .context("MCP 响应通道提前关闭")?
            .map_err(anyhow::Error::msg)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn write_message(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message).context("序列化 MCP JSON-RPC 失败")?;
        bytes.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&bytes)
            .await
            .context("写入 MCP stdin 失败")?;
        stdin.flush().await.context("刷新 MCP stdin 失败")
    }

    async fn dispatch(&self, message: Value) {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(sender) = self.pending.lock().await.remove(&id) else {
            return;
        };
        let result = if let Some(result) = message.get("result") {
            Ok(result.clone())
        } else {
            let error = message
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!({"message": "缺少 result/error"}));
            Err(error.to_string())
        };
        let _ = sender.send(result);
    }

    async fn fail_pending(&self, message: String) {
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for (_, sender) in pending {
            let _ = sender.send(Err(message.clone()));
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.get("text")
                            .and_then(Value::as_str)
                            .map_or_else(|| item.to_string(), str::to_owned)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string());
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!("MCP 工具返回错误：{content}");
        }
        Ok(content)
    }

    async fn shutdown(&self) {
        let _ = self.stdin.lock().await.shutdown().await;
        {
            let mut child = self.child.lock().await;
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
                Err(error) => warn!(%error, "查询 MCP server 状态失败"),
            }
        }
        if let Some(task) = self.reader_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.lock().await.take() {
            task.abort();
        }
        self.fail_pending("MCP client 已关闭".to_owned()).await;
    }
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    client: std::sync::Weak<McpClient>,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    while let Some(frame) = read_frame(&mut reader).await? {
        let message: Value = serde_json::from_slice(&frame).context("解析 MCP JSON-RPC 失败")?;
        let Some(client) = client.upgrade() else {
            break;
        };
        client.dispatch(message).await;
    }
    Ok(())
}

async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut first = String::new();
    loop {
        first.clear();
        if reader.read_line(&mut first).await? == 0 {
            return Ok(None);
        }
        if !first.trim().is_empty() {
            break;
        }
    }
    if let Some(length) = first.trim().strip_prefix("Content-Length:").map(str::trim) {
        let length = length.parse::<usize>().context("非法 MCP Content-Length")?;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await? == 0 {
                bail!("MCP header 未完整结束");
            }
            if header.trim().is_empty() {
                break;
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).await?;
        Ok(Some(body))
    } else {
        Ok(Some(first.trim_end().as_bytes().to_vec()))
    }
}

fn parse_remote_tools(result: &Value) -> Result<Vec<RemoteTool>> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .context("tools/list 响应缺少 tools 数组")?;
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .context("MCP tool 缺少 name")?;
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            if !input_schema.is_object() {
                bail!("MCP tool {name} 的 inputSchema 必须是对象");
            }
            Ok(RemoteTool {
                name: name.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("外部 MCP 工具")
                    .to_owned(),
                input_schema,
            })
        })
        .collect()
}

enum ConfigLoad {
    Disabled,
    FileError(String),
    Servers {
        valid: Vec<ServerConfig>,
        invalid: Vec<McpServerStatus>,
    },
}

async fn load_config(workspace: &Path) -> ConfigLoad {
    let path = workspace.join(".my-agent/mcp.json");
    if !path.exists() {
        return ConfigLoad::Disabled;
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ConfigLoad::FileError(format!("读取 {} 失败：{error}", path.display()));
        }
    };
    let root: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return ConfigLoad::FileError(format!("解析 {} 失败：{error}", path.display()));
        }
    };
    let Some(servers) = root.get("mcpServers").and_then(Value::as_object) else {
        return ConfigLoad::FileError("mcp.json 顶层必须包含对象 mcpServers".to_owned());
    };
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for (name, value) in servers {
        match parse_server_config(name, value, workspace) {
            Ok(config) => valid.push(config),
            Err(error) => invalid.push(McpServerStatus {
                name: name.clone(),
                connected: false,
                tools: Vec::new(),
                error: Some(format!("配置无效：{error:#}")),
            }),
        }
    }
    ConfigLoad::Servers { valid, invalid }
}

fn parse_server_config(name: &str, value: &Value, workspace: &Path) -> Result<ServerConfig> {
    let object = value.as_object().context("server 条目必须是对象")?;
    let command = required_string(object, "command")?;
    let args = optional_string_array(object, "args")?
        .into_iter()
        .map(|value| expand_placeholders(&value, workspace))
        .collect::<Result<Vec<_>>>()?;
    let env = optional_string_map(object, "env")?
        .into_iter()
        .map(|(key, value)| Ok((key, expand_placeholders(&value, workspace)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    let cwd = object
        .get("cwd")
        .map(|value| {
            let value = value.as_str().context("cwd 必须是字符串")?;
            let expanded = expand_placeholders(value, workspace)?;
            let path = PathBuf::from(expanded);
            let candidate = if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            };
            let canonical = std::fs::canonicalize(&candidate)
                .with_context(|| format!("无法解析 cwd：{}", candidate.display()))?;
            if !canonical.starts_with(workspace) {
                bail!("cwd 必须位于工作区内：{}", canonical.display());
            }
            Ok(canonical)
        })
        .transpose()?
        .unwrap_or_else(|| workspace.to_path_buf());
    Ok(ServerConfig {
        name: name.to_owned(),
        command,
        args,
        env,
        cwd,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{key} 必须是非空字符串"))
}

fn optional_string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .context("args 必须是字符串数组")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("args 必须是字符串数组")
        })
        .collect()
}

fn optional_string_map(object: &Map<String, Value>, key: &str) -> Result<HashMap<String, String>> {
    let Some(value) = object.get(key) else {
        return Ok(HashMap::new());
    };
    value
        .as_object()
        .context("env 必须是字符串 map")?
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_owned()))
                .context("env 值必须是字符串")
        })
        .collect()
}

fn expand_placeholders(input: &str, workspace: &Path) -> Result<String> {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').context("占位符缺少右花括号")?;
        let key = &after[..end];
        if key.is_empty() {
            bail!("占位符名称不能为空");
        }
        let value = if key == "WORKSPACE_ROOT" {
            workspace.to_string_lossy().into_owned()
        } else {
            std::env::var(key).with_context(|| format!("占位符环境变量未设置：{key}"))?
        };
        output.push_str(&value);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn bridge_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool))
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use crate::context::{ContextConfig, ContextManager};
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Message, Provider, Response, Role, ToolCall};
    use crate::safety::Approval;
    use crate::tools::ToolRegistry;

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct SwitchApproval {
        approved: AtomicBool,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Approval for SwitchApproval {
        async fn request(&self, _prompt: &str) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.approved.load(Ordering::SeqCst))
        }
    }

    struct ToolCallingProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for ToolCallingProvider {
        async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Response> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                assert!(tools.iter().any(|tool| tool.name == "mcp__echo__echo"));
                return Ok(Response::ToolCalls(vec![ToolCall {
                    id: "remote-call".to_owned(),
                    name: "mcp__echo__echo".to_owned(),
                    arguments: json!({"text": "hello"}),
                }]));
            }
            assert!(messages.iter().any(|message| {
                message.role == Role::Tool
                    && message.content.as_deref() == Some("echo-result")
                    && message.name.as_deref() == Some("mcp__echo__echo")
            }));
            Ok(Response::Text("模型收到 MCP 结果".to_owned()))
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("my-agent-mcp-{}-{name}-{id}", std::process::id()));
        std::fs::create_dir_all(path.join(".my-agent")).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    fn write_echo_server(workspace: &Path) -> (PathBuf, PathBuf) {
        let script = workspace.join("echo-server.sh");
        let pid = workspace.join("server.pid");
        std::fs::write(
            &script,
            r#"echo $$ > "$1"
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"1"}}}'
IFS= read -r initialized
IFS= read -r list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo tool","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}}]}}'
IFS= read -r call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echo-result"}]}}'
while IFS= read -r ignored; do :; done
"#,
        )
        .unwrap();
        (script, pid)
    }

    fn write_config(workspace: &Path, script: &Path, pid: &Path) {
        let config = json!({
            "mcpServers": {
                "broken": {"command": 42},
                "echo": {
                    "command": "/bin/sh",
                    "args": [script, pid],
                    "env": {"MCP_WORKSPACE": "${WORKSPACE_ROOT}"},
                    "cwd": "${WORKSPACE_ROOT}"
                }
            }
        });
        std::fs::write(
            workspace.join(".my-agent/mcp.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn isolates_bad_server_bridges_tool_requires_approval_and_cleans_child() {
        let workspace = workspace("bridge");
        let (script, pid_path) = write_echo_server(&workspace);
        write_config(&workspace, &script, &pid_path);
        let approval = Arc::new(SwitchApproval {
            approved: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        });
        let safety = Arc::new(SafetyPolicy::new(&workspace, approval.clone()).unwrap());
        let manager = Arc::new(McpManager::new(&workspace, safety));
        manager.reload().await;

        let status = manager.status();
        assert_eq!(status.servers.len(), 2);
        assert!(status.servers.iter().any(|server| {
            server.name == "broken" && !server.connected && server.error.is_some()
        }));
        assert!(status.servers.iter().any(|server| {
            server.name == "echo" && server.connected && server.tools == ["mcp__echo__echo"]
        }));

        let mut registry = ToolRegistry::new();
        registry.register_dynamic_source(manager.clone());
        let denied = registry
            .execute("mcp__echo__echo", json!({"text": "hello"}))
            .await;
        assert!(denied.is_err());
        approval.approved.store(true, Ordering::SeqCst);

        let provider = Arc::new(ToolCallingProvider {
            calls: AtomicUsize::new(0),
        });
        let context = ContextManager::new(
            provider.clone(),
            &workspace,
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let engine = LoopEngine::ephemeral(provider, registry, context, 3);
        let result = engine
            .run_turn(&mut Vec::new(), "使用 echo".to_owned())
            .await
            .unwrap();
        assert_eq!(result, "模型收到 MCP 结果");
        assert_eq!(approval.calls.load(Ordering::SeqCst), 2);

        let pid = std::fs::read_to_string(&pid_path).unwrap();
        manager.shutdown().await;
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(!alive, "MCP 子进程在 shutdown 后仍存活");
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn file_level_error_disables_mcp_without_failing_reload() {
        let workspace = workspace("bad-file");
        std::fs::write(workspace.join(".my-agent/mcp.json"), b"not-json").unwrap();
        let safety = Arc::new(
            SafetyPolicy::new(
                &workspace,
                Arc::new(SwitchApproval {
                    approved: AtomicBool::new(true),
                    calls: AtomicUsize::new(0),
                }),
            )
            .unwrap(),
        );
        let manager = McpManager::new(&workspace, safety);
        manager.reload().await;
        assert!(manager.status().file_error.is_some());
        assert!(manager.specs().is_empty());
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn reads_content_length_and_newline_frames() {
        let payload = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let (mut writer, reader) = tokio::io::duplex(1024);
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
            .await
            .unwrap();
        writer.write_all(payload).await.unwrap();
        writer.write_all(b"{\"next\":true}\n").await.unwrap();
        let mut reader = BufReader::new(reader);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), payload);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            b"{\"next\":true}"
        );
    }

    #[test]
    fn expands_only_allowed_config_values() {
        let workspace = workspace("placeholder");
        let value = json!({
            "command": "${WORKSPACE_ROOT}/bin/server",
            "args": ["${WORKSPACE_ROOT}/data"],
            "env": {"KEY_${WORKSPACE_ROOT}": "${WORKSPACE_ROOT}"},
            "cwd": "${WORKSPACE_ROOT}"
        });
        let parsed = parse_server_config("demo", &value, &workspace).unwrap();
        assert_eq!(parsed.command, "${WORKSPACE_ROOT}/bin/server");
        assert!(parsed.args[0].starts_with(workspace.to_string_lossy().as_ref()));
        assert!(parsed.env.contains_key("KEY_${WORKSPACE_ROOT}"));
        let _ = std::fs::remove_dir_all(workspace);
    }
}
