<p align="center">
  <img src="./docs/readme-hero.svg" alt="my-agent — Rust 个人编码 Agent" width="100%">
</p>

<p align="center">
  <img alt="Rust 1.88+" src="https://img.shields.io/badge/Rust-1.88%2B-0e716c?style=flat-square&logo=rust&logoColor=white">
  <img alt="Platform macOS and Linux" src="https://img.shields.io/badge/Platform-macOS%20%7C%20Linux-355e91?style=flat-square">
  <img alt="Providers OpenAI Anthropic Ollama" src="https://img.shields.io/badge/Provider-OpenAI%20%7C%20Anthropic%20%7C%20Ollama-ae6c19?style=flat-square">
  <img alt="ACP v1" src="https://img.shields.io/badge/ACP-v1-6d5bd0?style=flat-square">
  <img alt="MCP stdio" src="https://img.shields.io/badge/MCP-stdio-60717c?style=flat-square">
</p>

<p align="center">
  <strong>一个面向个人开发者的、本地优先的 AI 编码 Agent。</strong><br>
  用一个工作区 daemon 统一管理会话、上下文、计划、审批、工具执行和事件恢复。
</p>

<p align="center">
  <a href="#为什么是-my-agent">为什么</a> ·
  <a href="#核心能力">核心能力</a> ·
  <a href="#系统如何工作">架构</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#安全边界">安全边界</a> ·
  <a href="./docs/agent-system.html">完整系统说明</a>
</p>

---

## 为什么是 my-agent

很多 Agent 原型能“调用一次工具”，却很难稳定处理真实编码任务：长任务会失控、连接断开会丢状态、多个窗口会串会话、危险命令缺少统一边界，用户也看不出 Agent 究竟还在运行还是已经卡住。

`my-agent` 把这些问题收拢到一个 Rust 单 crate 中：

- **一个状态真相**：每个工作区只有 daemon 持有运行时状态，所有入口共享同一协议。
- **四类交互入口**：全屏 TUI、CLI、本地 HTTP/WebSocket、标准 ACP v1 stdio。
- **可靠的长任务循环**：主任务没有固定轮次硬上限，但有进度检查、重复检测、工具失败熔断和显式取消。
- **可恢复、可审计**：稳定 session、append-only JSONL、活动事件回放、按 session/request 过滤日志。
- **个人版的安全克制**：灾难命令硬拒，高风险操作审批，Cron 无人值守时安全拒绝。

> 想先看完整流程图和功能全景？打开 [项目系统说明](./docs/agent-system.html)。

## 终端体验

<p align="center">
  <img src="./docs/tui-preview.png" alt="my-agent 全屏终端 TUI" width="100%">
</p>

TUI 默认继承当前终端主题，也可启用内置 `dark` / `light` 语义色板。它不是简单的日志滚屏：

- 层级化 transcript、CJK 安全编辑、多行输入、历史草稿和发送队列。
- 工具调用默认聚合；`Ctrl+T` 锚定最近一条原 Query，在原对话中内联展开详情。
- 模型等待、流式输出、工具执行期间持续显示不确定进度动画。
- 审批、完成、失败、取消和连接中断都有明确终态；滚离底部时提示新消息。
- `Ctrl+/` 查看快捷键，`Ctrl+C` 取消当前请求，`/resume` 恢复历史会话。

## 核心能力

| 能力 | 当前实现 |
|---|---|
| **多 Provider** | OpenAI Chat Completions、Anthropic Messages、Ollama；协议差异封装在适配器内，统一输出严格 tool-call 生命周期事件。 |
| **8 个内置工具** | `read_file`、`write_file`、`edit_file`、`exec`、`remember`、`recall_memory`、`plan`、`sub_agent`。 |
| **计划与子 Agent** | 可重写、可持久化任务计划；子 Agent 使用全新历史、受限工具、最多 15 轮预算且不能递归派生。 |
| **图片与 PDF** | PNG/JPEG/WebP 可作为视觉内容块；PDF 在本地抽取最多 50 页文字；不支持时给出明确降级。 |
| **三条记忆链路** | 独立 session JSONL、60%/85% 两级上下文摘要、带 TTL 的关键词/中文 bigram 长期记忆。 |
| **Skill** | `.my-agent/skills/*.md` 使用 YAML frontmatter 与 semver，按当前请求稳定排序并按需加载正文。 |
| **Cron / Heartbeat** | interval/五段 cron、独立 session、有限指数退避、无人值守安全拒绝；heartbeat 不调用模型。 |
| **MCP stdio** | 本地 server 握手、工具发现、动态桥接、默认审批、错误隔离和子进程清理。 |
| **多窗口隔离** | 每个 TUI/REPL/ACP 窗口拥有独立 session；历史、活动请求、审批、取消和订阅互不串线。 |
| **可观测性** | round、Provider 首增量/总耗时、工具耗时与成功状态关联 `session_id/request_id` 写入工作区日志。 |

## 系统如何工作

```mermaid
flowchart TB
    subgraph Entry[四类交互入口]
        TUI[全屏 TUI]
        CLI[CLI / REPL]
        API[HTTP + SSE / WebSocket]
        ACP[ACP v1 stdio]
    end

    TUI --> Client[DaemonClient\nNDJSON JSON-RPC]
    CLI --> Client
    API --> Client
    ACP --> Client
    Client --> Daemon[工作区 Daemon\n状态唯一真相]

    Daemon --> Session[Session / Plan / Memory]
    Daemon --> Control[Approval / Cancel / Replay]
    Daemon --> Engine[LoopEngine ReAct]

    Engine --> Context[稳定前缀 + 历史 + 动态上下文]
    Context --> Provider[OpenAI / Anthropic / Ollama]
    Provider --> Assembler[Canonical Tool-call Assembler]
    Assembler --> Safety[Schema + Safety + Approval]
    Safety --> Tools[只读并行 / 副作用串行]
    Tools --> Engine
    Engine --> Persist[回答与事件持久化]
    Persist --> Client
```

一次请求的核心链路：

1. 任一入口把请求交给 `DaemonClient`，入口本身不创建 Provider 或工具运行时。
2. daemon 按 session 获取可取消的 turn 锁，追加用户消息并登记活动请求。
3. 上下文按“稳定前缀 → 历史 → 动态信息”组装，超过水位时进行两级压缩。
4. Provider 流式返回文本或工具调用，唯一 assembler 严格拼装参数并 fail-closed。
5. 整批工具先做 JSON Schema 与安全决策，再按只读并行、副作用串行执行。
6. 结果用原始 `tool_call_id` 回填；最终回答落盘，并投影为各入口需要的事件格式。

## 快速开始

### 1. 构建

需要 Rust `1.88+`。当前进程间通信使用 Unix Domain Socket，支持 macOS 和 Linux。

```bash
git clone https://github.com/wangyichen666/agent-daemon.git
cd agent-daemon
cargo build --release
```

### 2. 配置 Provider

OpenAI 兼容服务示例：

```bash
export API_TYPE='openai-chat'
export OPENAI_API_KEY='你的密钥'
export OPENAI_BASE_URL='https://api.deepseek.com'
export MODEL_NAME='deepseek-chat'
```

Anthropic Messages 使用同一组通用环境变量，将 `API_TYPE` 改为 `anthropic-messages`，并填写对应的 API URL、密钥和模型名。

本地 Ollama 不需要 API Key：

```bash
export API_TYPE='ollama'
export MODEL_NAME='qwen3'
# OPENAI_BASE_URL 未设置时默认 http://127.0.0.1:11434
```

### 3. 检查并启动

```bash
./target/release/my-agent config check

# 默认进入全屏 TUI，并自动拉起当前工作区 daemon
./target/release/my-agent
```

也可以安装到 Cargo bin：

```bash
cargo install --path .
my-agent
```

## 入口与命令

| 命令 | 用途 |
|---|---|
| `my-agent` / `my-agent tui` | 启动全屏终端界面。 |
| `my-agent chat` | 启动普通 REPL。 |
| `my-agent chat "检查项目"` | 发起一次性请求。 |
| `my-agent serve --bind 127.0.0.1:8787` | 提供 OpenAI 兼容 HTTP/SSE 与 `/ws`。 |
| `my-agent editor` | 启动标准 ACP v1 stdio server。 |
| `my-agent status` | 查看当前工作区 daemon 与日志路径。 |
| `my-agent sessions` | 列出稳定 session、摘要和运行状态。 |
| `my-agent logs --lines 100` | 查看最近 daemon 日志。 |
| `my-agent logs --session … --request …` | 按 session/request 精确排障。 |
| `my-agent stop` | 优雅停止当前工作区 daemon。 |

交互入口共享以下 Slash 命令：

```text
/help      /status    /sessions   /resume [编号|ID]
/new       /cancel    /skill      /cron
/mcp       /ping      /exit
```

<details>
<summary><strong>HTTP / SSE / WebSocket 示例</strong></summary>

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"你的模型名","messages":[{"role":"user","content":"检查当前项目"}]}'
```

- `/v1/chat/completions` 支持普通 JSON 和 OpenAI 风格 SSE。
- `/ws` 首帧发送 `{"type":"connect","token":"..."}`，之后使用全双工 JSON-RPC 2.0。
- 非回环监听必须设置 `MY_AGENT_API_TOKEN`。
- HTTP 无法弹出终端审批，因此需要审批的操作会安全拒绝。

</details>

<details>
<summary><strong>Cron 与 MCP 配置示例</strong></summary>

Cron：

```text
/cron add nightly cron=0,2,*,*,* --retries=2 --backoff=10 检查项目并生成报告
/cron add quick interval=300 执行轻量巡检
/cron list
/cron run-now quick
/cron disable quick
/cron remove quick --confirm
```

`.my-agent/mcp.json`：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "${WORKSPACE_ROOT}"],
      "env": {},
      "cwd": "${WORKSPACE_ROOT}"
    }
  }
}
```

保存后运行 `/mcp reload`。当前只支持可信的本地 stdio MCP server，不支持远程 streamable-http/SSE transport。

</details>

## 可靠性设计

- **严格工具装配**：乱序、重复完成、缺失参数或不完整 EOF 会整轮拒绝，不执行半批副作用。
- **受控长任务**：主任务没有固定 ReAct 轮次上限，每 50 轮检查进度；完全相同调用与结果连续 10 次才按无进展熔断。
- **失败恢复**：工具失败会回填模型修复；连续 3 次工具失败则终止并返回明确原因。
- **断线恢复**：daemon 为活动请求保留最多 1 MiB 事件回放；重新 `session.load` + `agent.subscribe` 可继续接收输出和审批。
- **平滑升级**：ready 标记记录可执行文件内容指纹；重新构建后会优雅停止旧 daemon，再使用新版本启动。
- **进程清理**：`exec` 默认 300 秒超时；取消或超时会清理整个子进程组。

## 安全边界

这是面向个人工作区的**软安全边界**，不是容器、namespace 或 seccomp 级沙箱。

| 操作 | 策略 |
|---|---|
| 工作区内读取 | 直接允许，可进入最多 8 路只读并行波次。 |
| 工作区外读取 | 直接拒绝。 |
| 工作区外写入/编辑 | 请求人工审批，默认拒绝。 |
| `rm -rf /`、`mkfs`、块设备覆盖、fork 炸弹等 | 硬拒绝。 |
| `kill`、`sudo`、`git reset --hard`、`cargo publish` 等 | 请求人工审批。 |
| Cron 中任何需审批动作 | 无人值守安全拒绝。 |
| 非回环 HTTP/WebSocket | 必须配置 Bearer Token。 |

MCP server 以当前用户权限运行，只应连接可信本地配置。图片/PDF 限制 16 MiB；PDF 最多抽取 50 页和约 512K 字符，不做视觉渲染。

## 配置参考

<details>
<summary><strong>常用环境变量</strong></summary>

| 环境变量 | 默认值 | 作用 |
|---|---:|---|
| `API_TYPE` | `openai-chat` | `openai-chat`、`anthropic-messages` 或 `ollama`。 |
| `OPENAI_API_KEY` | 无 | OpenAI/Anthropic 通用密钥变量；Ollama 不需要。 |
| `OPENAI_BASE_URL` | Ollama 为 `http://127.0.0.1:11434` | Provider 服务根 URL。 |
| `MODEL_NAME` | 无 | 模型名称。 |
| `CONTEXT_TOKEN_BUDGET` | `32000` | 上下文 token 预算。 |
| `CONTEXT_RECENT_MESSAGES` | `12` | 强压缩时保留的最近消息数。 |
| `CONTEXT_MILD_PERCENT` | `60` | 温和压缩触发水位。 |
| `CONTEXT_STRONG_PERCENT` | `85` | 强力压缩触发水位。 |
| `MULTIMODAL_ENABLED` | 按模型名检测 | 显式开启/关闭图片内容块。 |
| `SKILLS_DIR` | `.my-agent/skills` | 本地 Skill 目录。 |
| `MY_AGENT_API_TOKEN` | 未设置 | 非回环 HTTP/WebSocket 的 Bearer Token。 |
| `MY_AGENT_TUI_THEME` | `terminal` | `terminal`、`dark` 或 `light`。 |
| `MY_AGENT_TUI_MOUSE` | 未设置 | 设为 `1` 时捕获鼠标滚轮。 |
| `MY_AGENT_EXEC_TIMEOUT_SECS` | `300` | `exec` 单次最长运行秒数。 |
| `RUST_LOG` | `info` | daemon 日志过滤级别。 |

</details>

## 项目结构

<details>
<summary><strong>展开模块说明</strong></summary>

```text
src/main.rs                Clap 子命令与启动分发
src/client.rs              Unix / 内存 DaemonClient
src/daemon/                状态、协议、审批、运行时、生命周期、server
src/entry/                 TUI、CLI、HTTP/WS、ACP 与恢复适配
src/provider.rs            Provider 公共契约与 execution identity
src/provider/              OpenAI、Anthropic、Ollama 适配器
src/tool_calls.rs          canonical tool-call assembler
src/loop_engine.rs         ReAct、取消、并行波次、熔断与结果回填
src/context.rs             上下文排序、Skill、估算与两级压缩
src/session.rs             稳定 session 与 append-only JSONL
src/memory.rs              TTL 长期记忆与关键词/bigram 召回
src/plan.rs                原子持久化计划
src/sub_agent.rs           隔离上下文的受限子 Agent
src/skills.rs              版本化 Skill 索引与按需加载
src/cron.rs                Cron、重试与 heartbeat
src/mcp.rs                 MCP stdio 客户端与工具桥接
src/safety.rs              路径与命令安全决策点
src/tools/                 内置工具注册、校验与执行
```

</details>

## 开发与验证

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

测试使用本地 mock Provider，不需要真实 API Key。真实模型端到端测试需要自行提供对应服务配置。

## 当前边界

项目目前定位为个人、本地优先的编码 Agent，暂不提供：

- 多租户与 RBAC
- 容器或操作系统级沙箱
- 向量数据库与自动 embedding 召回
- 远程 MCP transport
- Windows Named Pipe

---

<p align="center">
  <strong>my-agent</strong> · Rust 构建 · daemon 驱动 · 本地优先<br>
  <a href="./docs/agent-system.html">查看完整系统说明</a>
</p>
