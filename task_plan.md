# 个人级 AI 编码 Agent 实施计划

## 目标

使用 Rust 从零构建一个结构清晰、个人可用、单进程 CLI 形态的 AI 编码 Agent。严格按阶段实现并验证，核心版覆盖需求中的 11 个处理环节。

## 阶段

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 0. 项目骨架 | complete | binary crate、模块目录、基础配置 |
| 1. ReAct 循环 | complete | OpenAI 兼容 Provider、read_file/exec、工具分发、REPL |
| 2. 安全与并行 | complete | 参数校验、路径策略、命令安全、审批、并行波次、write/edit |
| 3. 上下文工程 | complete | 上下文分块、环境注入、token 估算、递归压缩 |
| 4. 持久化与会话 | complete | session.jsonl、恢复、会话串行锁 |
| 5. 个人增强 | complete | 关键词 + 中文 bigram 长期记忆（其余可选项不默认扩张） |
| 6. 全量验收 | complete | build、clippy、test、README、手工可验证说明 |

## 进阶增补（2026-09-08）

| 批次 / 功能 | 状态 | 主要交付 |
|---|---|---|
| 基线通读与偏差核对 | complete | 逐模块核实现状，记录影响后续设计的偏差 |
| 第一批 1. plan | complete | 可落盘计划状态、plan 工具、动态上下文注入 |
| 第一批 2. sub_agent | complete | 可复用执行入口、隔离历史、受限工具、15 轮预算、禁止递归 |
| 第一批 3. 前缀缓存 | complete | 稳定前缀排序、DeepSeek 自动缓存、OpenAI/DeepSeek usage 日志 |
| 第二批 4. 图片/PDF | complete | 图片内容块、可选多模态、PDF 本地文本抽取与限制 |
| 第二批 5. 重复检测 | complete | 调用/参数/结果指纹与建议性提示 |
| 第二批 6. 两级压缩 | complete | 60% 温和压缩、85% 强力压缩、环境可配置 |
| 第三批 7. skill | complete | Markdown 索引、关键词/bigram 匹配、按需正文注入 |
| 第三批 8. cron | deferred | 蓝图明确为第三批可停项；留作独立迭代，避免本轮扩大 REPL 并发状态面 |
| 第三批 9. MCP | deferred | 蓝图明确为第三批可停项；留作独立迭代，避免未经选型引入外部协议生命周期 |
| 进阶全量验收 | complete | release build、36 项测试、严格 clippy、格式、CLI 烟雾测试与文档 |

### 进阶实施原则

- 在现有模块边界上增量重构，不重写核心。
- 第一批、第二批全部实现；第三批先实现 skill，cron/MCP 在前两批稳定后再按复杂度评估。
- sub_agent 默认受限工具集，不持久化到主 session，且不暴露自身，递归深度固定为 1。
- 多模态优先保持 OpenAI 兼容；PDF 采用本地开源解析，图片能力通过可选模型配置控制。

## 关键决策

- 项目目录本身作为 crate 根目录，不再嵌套一层 `my-agent/`。
- API 使用 OpenAI Chat Completions 兼容协议，配置全部来自环境变量。
- 安全审批由 CLI 回调提供；工具与主循环只依赖抽象接口。
- 测试使用 mock provider，不依赖真实密钥或网络。
- 阶段五只默认实现蓝图中最贴合个人版定位的长期记忆；cron、子 Agent、MCP 保留扩展点，不增加本次核心复杂度。

## Daemon + 三入口演进（2026-09-08）

### 目标

在不重写 ReAct、安全、记忆、计划、上下文与工具逻辑的前提下，把运行时状态收拢到 daemon，提供共享 JSON-RPC 协议、Unix socket 客户端、瘦 CLI、本地 HTTP API 和专业启动自检。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 基线通读与偏差核对 | complete | 核实现有所有权、审批、session、取消和流式能力 |
| A. 协议与 daemon 内核 | complete | 协议 SSOT、DaemonState、handlers、进程内 client 回环 |
| B. daemon/socket/启动优化 | complete | UDS、生命周期探测与自动拉起、clap 子命令、配置检查、瘦 REPL |
| C. 本地 HTTP API | complete | 127.0.0.1 默认绑定、health、OpenAI 兼容 chat/SSE、非回环 token 门禁 |
| D. 编辑器 stdio 骨架 | complete | JSON-RPC stdin/stdout 适配器，复用 DaemonClient |
| 全量验收 | complete | 48 项回归、跨入口一致性、release/clippy/fmt、配置诊断、HTML 桌面交付 |

### 初始设计约束

- daemon 是历史、计划、审批、取消和运行中 turn 的唯一真相源；入口不得直接持有 LoopEngine。
- 阶段 A 先用内存双向通道验证协议与 handler，再替换为 Unix socket，避免同时调试所有权和传输。
- 流式事件必须从 LoopEngine/Provider 的共享层产生，不能由 CLI 或 HTTP 入口伪造另一套执行逻辑。
- 正在执行的 turn 不因挂钟超时被强杀；取消使用显式 cancellation token/协作式检查。
- 每阶段完成后先 build、test、严格 clippy，再进入下一阶段。

## 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| plan 首轮 clippy 报 `PlanStore::in_memory` 为 dead code | 1 | 该构造器只用于单元测试，限定为 `#[cfg(test)]` 后重跑全套验证 |
| sub_agent 首次大补丁被 `apply_patch` 拒绝（同一文件重复 Update 段） | 1 | 补丁未落盘；改为每个文件单一 Update 段的原子补丁 |
| sub_agent 编译时报并行波次闭包 `FnOnce` 生命周期不够通用 | 1 | 并行迭代改为持有 `ToolCall` 克隆值，避免 `async_trait` future 跨层借用切片元素 |
| 多模态整体替换补丁因同文件 Delete/Add 被拒绝 | 1 | 补丁其余段先落盘、read.rs 被明确删除；随后单独新增完整 read.rs |
| 多模态验证 clippy 报 `ReadFileTool::new` 为 dead code | 1 | 正式入口使用 `from_env`，将无配置构造器限定为测试代码 |
| skill 首次编译时 `filter_map` 参数多标了一层引用 | 1 | 将 `&&Skill` 修正为迭代器实际产出的 `&Skill` |
| CUA 浏览器安全策略阻止打开本地 `file://` HTML | 1 | 不绕过策略；改做 xmllint 静态解析与源码一致性检查，并在交付说明视觉复检未执行 |
| 初始目录不是 Git 仓库，`git status` 失败 | 1 | 仅记录；构建任务不依赖 Git 仓库 |
| planning-with-files 技能声明的 templates 目录不存在 | 1 | 按技能定义的用途自行创建三个计划文件 |
| crates.io 索引更新超过两分钟无响应 | 1 | 用户建议使用中国源；探测 rsproxy 与 USTC 均可达，项目级切换至 rsproxy sparse，不改全局配置 |
| `tokio::io::stdin` 未启用 `io-std` | 1 | 在 Tokio features 中补充 `io-std` 后重新验证 |
| Clippy `borrowed_box` 拒绝显式 `&Box<dyn Tool>` | 1 | 让迭代器闭包推断引用类型，消除多余 Box 借用表达 |
| 阶段二首次测试 2 项失败、Clippy 3 项告警 | 1 | 支持 `mkfs.*` 变体；把读取夹具移入工作区；移除未使用方法；修正借用与导入位置 |
| 阶段三首次大补丁上下文不匹配 | 1 | 无文件被部分修改；拆分为独立小补丁依次接入 |
| token 估算泛型不能接收未定长切片 | 1 | 为序列化泛型增加 `?Sized` 边界 |
| 阶段四严格 Clippy 检出未使用的 `Message` 导入 | 1 | 删除已被会话恢复类型推断替代的导入 |
| 直接删除端到端验证临时文件被执行策略拒绝 | 1 | 改为移动到 macOS 废纸篓，可恢复且不再留在 `/tmp` |
| 进阶计划首次补丁因进度文件上下文不匹配而未应用 | 1 | 先读取文件末尾，再按现有内容拆分追加 |
| 最终 stdio 冒烟测试帧漏写 `jsonrpc` | 1 | 适配器正确返回 -32700；补齐 `jsonrpc: "2.0"` 后请求 ID、响应与空闲退出验证通过 |
| ACP crate 首次 `cargo search/info` 被项目 source replacement 拒绝 | 1 | 按 Cargo 提示改用显式 `--registry crates-io` 查询，不重复原命令 |
| ACP 首次编译出现 `ConnectionTo` 借用后移动 2 处及未使用导入 | 1 | `spawn` 前克隆连接供后台任务持有，并移除多余 `ConnectTo` 导入 |
| ACP 恢复整块补丁因 fmt 后锚点变化未匹配 | 1 | 补丁未落盘；读取当前片段后拆为 import、load handler、helper 三个小补丁 |
| Cargo 测试命令误传三个位置过滤器 | 1 | Cargo 尚未编译源码；改用单个 `--all-targets` 全量测试覆盖相关模块 |

## 标准 ACP + WebSocket + 重连恢复（2026-09-08）

### 目标

在现有 daemon 单一真相源上，把编辑器 stdio 私有透传升级为标准 ACP server，为 axum Web 服务新增全双工 WebSocket 私有 RPC 通道，并让 CLI、ACP、WebSocket 在连接或重连后恢复未决审批，且不改变 HTTP/SSE 兼容入口。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 0. 基线与 ACP crate 调研 | complete | RPC/事件/审批真实结构、入口行为、官方 `agent-client-protocol =2.1.0` |
| A. 标准 ACP 入口 | complete | 官方 SDK、initialize/session 映射、typed update/permission、正式 Client 集成测试 |
| B. WebSocket 全双工入口 | complete | `/ws`、connect 鉴权、ID 映射、RPC/Event 双向审批、HTTP/SSE 回归 |
| C. 三入口重连恢复 | complete | daemon 可订阅事件真相、公共恢复 helper、CLI/ACP/WS pending 与 active 恢复 |
| D. 全量验收与交付 | complete | fmt/release/test/clippy、三入口断线恢复证据、README/HTML、提交推送 |

### 本轮约束

- 入口只翻译协议；session、approval、cancel 与请求终态仍只由 daemon 决定。
- 客户端断开、缺 ACK 或超时均不得自动批准、拒绝或清空 pending 状态。
- 优先采用固定版本的活跃开源 ACP crate；若实际 API 无法满足，再依据标准规范手写并记录原因。
- HTTP `/health`、`/v1/chat/completions` 及 SSE 保持兼容。

### 阶段 C/D 验收补充

- active turn 使用最多 1 MiB 回放缓存 + broadcast 实时订阅；原连接断开不再影响审批等待。
- `agent.subscribe` 为重连入口，按当前 pending 集合过滤已解决的旧审批回放，避免 ACP 重复弹窗。
- `session.load` 改从 append-only JSONL 读取快照，审批等待期间不会被内存历史锁阻塞。
- CLI、标准 ACP、WebSocket 均有断线后恢复活动请求、审批与最终文本的集成测试。

## TUI 入口（2026-09-08）

### TUI 视觉优化（已完成）

- 显式深色背景与文字色、居中限宽、消息留白、Markdown 标题/强调/代码样式。
- 精简工具回执、独立审批区、输入光标与粘贴、按实际换行滚动。
- 使用 TestBackend 验证宽/窄终端和长文本，生成渲染预览；更新 release 与使用说明。
- 验证：59 项全量测试通过；补充修正后 TUI 测试、严格 Clippy、release 再次通过。实际 PTY 验证中文草稿、Esc 退出及终端 echo/icanon 恢复。
- 测试修正：宽字符的占位单元默认样式不代表可见字符颜色；渲染断言按可见字符检查。Swift 预览遇到系统 SDK 模块冲突，改用 Objective-C/AppKit 从 TestBackend 单元格生成 PNG。

### 目标

增加类似 Claude Code 的终端交互界面，但保持 TUI 为瘦客户端：不持有 Provider、LoopEngine、session 或审批真相，只通过已有 `DaemonClient` 调用 daemon。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| T1. 终端基础设施 | complete | ratatui/crossterm、终端原始模式、退出恢复、`tui` 子命令 |
| T2. 对话与事件流 | complete | 输入框、消息滚动、文本增量、工具状态和终态响应 |
| T3. 审批/取消/重连 | complete | y/N 审批、Ctrl-C 取消、session.load + agent.subscribe 恢复 |
| T4. 验收与文档 | complete | TUI 单元测试、命令文档、HTML/README 同步、全量构建验证 |

### 架构决策

- TUI 代码放在当前仓库的 `src/entry/tui.rs`，因为它是本项目的正式入口，需要与协议类型和 `DaemonClient` 一起版本化。
- TUI 不应复制 `LoopEngine` 或直接调用工具；daemon 仍是唯一真相源，未来 ACP/HTTP/CLI/TUI 共用同一套事件语义。
- 仅把终端绘制和用户输入放在 TUI；恢复、审批响应、取消和请求生命周期通过现有 RPC 完成。

### TUI 终端主题兼容修复（已完成）

- 默认改为终端原生主题：继承前景色与背景色，不再整屏强制 RGB 背景。
- 保留显式真彩深色主题，通过 `MY_AGENT_TUI_THEME=dark` 启用。
- 增加渲染测试，确保默认主题不写入固定背景色；fmt、60 项测试、严格 clippy、release 构建与实际 PTY 退出恢复均通过。
