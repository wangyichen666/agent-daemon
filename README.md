# my-agent

一个用 Rust 编写的个人级 AI 编码 Agent。一个工作区对应一个常驻 daemon；CLI、本地 OpenAI 兼容 HTTP API 和编辑器 stdio JSON-RPC 三个瘦入口，都通过同一协议访问 daemon 持有的会话、计划、审批、取消状态与 ReAct 运行时。

## 功能

- OpenAI Chat Completions 兼容 Provider：解析 SSE 文本增量及分片 function calling，并记录 OpenAI/DeepSeek 缓存命中字段。
- 全屏终端 TUI：默认继承终端前景/背景，兼容浅色、深色和自定义主题；可选真彩深色主题。界面居中限宽，支持 Markdown 标题/粗体/代码排版、工具详情收起、中文换行滚动、流式文本、独立审批面板和可见输入光标。
- 8 个工具：`read_file`、`write_file`、`edit_file`、`exec`、`remember`、`recall_memory`、`plan`、`sub_agent`。
- `read_file` 可把 PNG/JPEG/WebP 作为视觉内容块发送，并在本地抽取最多 50 页 PDF 文字。
- `plan` 管理可重写任务步骤；`sub_agent` 用全新历史、受限工具和最多 15 轮预算执行独立子任务，不能递归派生。
- `.my-agent/skills/*.md` 技能库只常驻标题/摘要索引，按关键词和中文 bigram 最多加载 3 个命中正文。
- 三条记忆链路：append-only 会话、60%/85% 两级上下文摘要、TTL 长期记忆。
- JSON Schema 参数校验、统一路径边界、灾难命令硬拒、跨工作区写入和高风险命令审批。
- 同轮连续只读工具最多 8 路并行，副作用工具串行；连续三次完全相同的工具调用与结果只做软提醒。
- 显式按 request id 取消；不使用挂钟超时强杀正在执行的 turn。
- daemon 使用工作区稳定哈希隔离 socket/PID/ready/log；启动探测、并发启动锁、失效标记清理和最后客户端断开后的空闲退出均已实现。
- 本地 HTTP 提供 `/health` 与 `/v1/chat/completions`，支持普通 JSON 与 SSE；同一服务的 `/ws` 提供全双工 JSON-RPC、事件流和交互审批；非回环监听必须配置 Bearer Token。
- 编辑器入口实现标准 ACP v1（`agent-client-protocol`），支持 initialize、session/new/load、prompt、cancel、工具更新和 typed 权限请求。
- 连接断开后可恢复：daemon 保留活动 turn、待审批和最多 1 MiB 事件回放；CLI、ACP `session/load`、WebSocket 重连均可继续消费，不会因断线自动批准或拒绝。

cron 和 MCP 仍是未实现的可选扩展。本项目也不提供多租户、RBAC、容器沙箱、向量数据库或企业连接器。

## 请求链路

```text
TUI / CLI / HTTP+WebSocket / 标准 ACP stdio
          │
          ▼
     DaemonClient
          │  NDJSON JSON-RPC / Unix Domain Socket
          ▼
     DaemonState（唯一真相）
          │  history / session / plan / approvals / cancellation
          ▼
      LoopEngine ReAct
          │
          ├─ 稳定前缀 → 历史 → 动态上下文 → 两级压缩
          ├─ Provider SSE → text.delta / tool calls
          ├─ 参数校验 → 安全决策 → 审批事件
          ├─ 只读并行 / 副作用串行 → tool_call_id 回填
          └─ assistant 落盘 → completed 事件 → 最终 Response
```

入口不创建 Provider、工具、安全策略或历史。模型文本增量、工具开始/完成、审批和终态都由共享执行层产生，再由 CLI、HTTP SSE 或 stdio 适配器展示。

## 模块

```text
src/main.rs                clap 子命令与启动分发
src/config.rs              配置聚合检查与首次使用提示
src/client.rs              内存/Unix DaemonClient、request_id 多路复用
src/daemon/
  mod.rs                   DaemonState：运行时状态唯一真相
  protocol.rs              JSON-RPC 请求/响应/事件帧 SSOT，4 MiB 上限
  handlers.rs              chat/session/approval/cancel/subscribe/stop 方法
  approval.rs              可挂起、可重连查看的审批中介
  runtime.rs               Provider、工具、上下文、会话统一装配
  lifecycle.rs             工作区运行目录、PID/ready、探测与自动拉起
  server.rs                内存回环与 Unix socket server
src/entry/
  cli.rs                   REPL、恢复活动请求、流式显示、slash 命令、Ctrl-C 取消
  tui.rs                   ratatui 全屏界面、输入框、事件流、审批和取消
  serve.rs                 health、OpenAI 兼容 HTTP/SSE 与全双工 WebSocket
  editor.rs                标准 ACP v1 stdio server、恢复与权限请求
src/entry/recovery.rs      三入口共享的 session.load、approval、active subscribe helper
src/provider.rs            Provider trait、OpenAI 兼容请求与 SSE
src/loop_engine.rs         ReAct、事件、取消、工具波次与结果回填
src/context.rs             上下文排序、环境、Skill、估算与压缩
src/safety.rs              文件与命令的唯一安全决策点
src/session.rs             JSONL 会话、备份列表与 RAII turn 锁
src/memory.rs              TTL 长期记忆与关键词/bigram 召回
src/plan.rs                当前计划及原子 JSON 持久化
src/sub_agent.rs           独立历史、受限工具的子 Agent
src/skills.rs              Markdown Skill 索引与按需加载
src/tools/                 Tool trait、注册表与文件/命令工具
```

## 构建与配置

需要 Rust 1.88 或更高版本（标准 ACP SDK 的 MSRV）。目前进程间传输使用 Unix Domain Socket，支持 macOS/Linux；Windows Named Pipe 留作后续适配。

```bash
cargo build --release

export OPENAI_API_KEY='你的密钥'
export OPENAI_BASE_URL='https://api.deepseek.com'
export MODEL_NAME='你的模型名'

./target/release/my-agent config check
```

密钥、服务地址和模型名不会硬编码。项目级 `.cargo/config.toml` 使用 rsproxy sparse 镜像，改善中国网络环境下的 crate 下载，不修改全局 Cargo 配置。

常用可选环境变量：

| 环境变量 | 默认值 | 作用 |
|---|---:|---|
| `CONTEXT_TOKEN_BUDGET` | `32000` | 上下文 token 预算，最小 256 |
| `CONTEXT_RECENT_MESSAGES` | `12` | 强压缩时保留的最近消息数 |
| `CONTEXT_MILD_PERCENT` | `60` | 温和压缩触发水位 |
| `CONTEXT_STRONG_PERCENT` | `85` | 强力压缩触发水位 |
| `SESSION_PATH` | `.my-agent/session.jsonl` | 会话 JSONL 路径 |
| `MEMORY_PATH` | `.my-agent/memory.jsonl` | 长期记忆路径 |
| `PLAN_PATH` | `.my-agent/plan.json` | 计划路径；`off` 表示仅内存 |
| `SKILLS_DIR` | `.my-agent/skills` | Markdown Skill 目录 |
| `MULTIMODAL_ENABLED` | 按模型名检测 | 显式启用/关闭图片内容块 |
| `MY_AGENT_RUNTIME_DIR` | 系统临时目录 | daemon socket/PID/ready/log 根目录 |
| `MY_AGENT_API_TOKEN` | 未设置 | HTTP Bearer Token；非回环监听必填 |
| `MY_AGENT_TUI_THEME` | `terminal` | TUI 主题；默认继承终端颜色，`dark` 启用内置真彩深色主题 |
| `RUST_LOG` | `warn` | tracing 日志过滤 |

## 使用

```bash
# 默认进入全屏 TUI；daemon 不存在时自动拉起
./target/release/my-agent

# 仍可使用普通 REPL
./target/release/my-agent chat

# 一次性提问
./target/release/my-agent chat "读取 README 并总结架构"

# 运行状态、会话与停止
./target/release/my-agent status
./target/release/my-agent sessions
./target/release/my-agent stop

# 本地 OpenAI 兼容 API
./target/release/my-agent serve --bind 127.0.0.1:8787
curl http://127.0.0.1:8787/health
# 全双工 WebSocket（首帧发送 {"type":"connect","token":"..."}）
# ws://127.0.0.1:8787/ws

# 编辑器标准 ACP v1 stdio server
./target/release/my-agent editor
```

REPL 支持 `/help`、`/status`、`/sessions`、`/new`、`/cancel`、`/exit`。运行中的 turn 按 Ctrl-C 会发送 `agent.cancel`，不会直接杀掉 daemon。

TUI 中 Enter 发送，Alt+Enter 换行，支持多行粘贴；PageUp/PageDown 查看历史，Ctrl+T 展开/收起工具详情，Ctrl+U 清空草稿，Ctrl+C 取消当前请求。Esc 随时退出界面，空闲时也可输入 `/exit`；退出保留 daemon 中尚在运行的任务。字母 `q` 作为正常文本输入。

TUI 默认使用 `terminal` 主题，不写固定前景/背景色，可跟随终端的浅色、深色或自定义配色。确认终端支持 truecolor 后，可用 `MY_AGENT_TUI_THEME=dark myagent` 启用内置深色主题；若显示异常，取消该变量或设为 `terminal`。

HTTP 请求示例：

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"你的模型名","messages":[{"role":"user","content":"检查当前项目"}]}'
```

HTTP 入口不能弹终端审批，因此遇到需要批准的操作会安全地自动拒绝；CLI 可以通过 daemon 审批事件交互确认。

## 安全边界

这是软安全边界，不是操作系统沙箱。`rm -rf /`、`mkfs`、块设备覆盖、fork 炸弹等会直接拒绝；`kill`、`sudo`、`git reset --hard`、跨工作区写入等会请求审批。Shell 命令仍以当前用户权限执行，应只在可信工作区运行。

图片限制 16 MiB，base64 只存在于当前 turn 的临时消息，不写 session。PDF 限制 16 MiB、50 页和约 512K 字符，不做视觉渲染。

## 验证

```bash
cargo fmt --all -- --check
cargo build --release
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

自动化测试使用 mock Provider，不需要 API 密钥；真实模型端到端验证需要可用的兼容服务配置。

# agent-daemon-
