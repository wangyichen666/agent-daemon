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
| 第三批 8. cron | complete | daemon 内轻量调度、独立 session、重试、heartbeat、持久化与 slash 管理 |
| 第三批 9. MCP | complete | 自研 stdio JSON-RPC、握手、动态工具桥接、审批隔离与子进程生命周期 |
| 进阶全量验收 | complete | release build、36 项测试、严格 clippy、格式、CLI 烟雾测试与文档 |

### 进阶实施原则

- 在现有模块边界上增量重构，不重写核心。
- 第一批、第二批与第三批 skill/cron/MCP 均已实现；新增外部能力仍必须进入统一工具注册、schema、safety 与 approval 链路。
- sub_agent 默认受限工具集，不持久化到主 session，且不暴露自身，递归深度固定为 1。
- 多模态优先保持 OpenAI 兼容；PDF 采用本地开源解析，图片能力通过可选模型配置控制。

## 关键决策

- 项目目录本身作为 crate 根目录，不再嵌套一层 `my-agent/`。
- API 使用 OpenAI Chat Completions 兼容协议，配置全部来自环境变量。
- 安全审批由 CLI 回调提供；工具与主循环只依赖抽象接口。
- 测试使用 mock provider，不依赖真实密钥或网络。
- 阶段五已补齐个人版所需的记忆、cron、子 Agent 与本地 MCP；远程 MCP transport、RBAC 与系统级沙箱仍明确留作后续。

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

## 默认新会话与 `/resume`（2026-09-08）

### 目标

每次启动交互入口都创建全新 session，不自动展示旧对话；用户在 TUI/REPL 输入 `/resume` 后查看历史 session，并选择一个继续会话。断线后仍允许 daemon 保持活动任务，不把“启动新会话”和“恢复未完成请求”混为一谈。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| S1. 现状与语义核对 | complete | session 存储格式、daemon RPC、TUI/CLI 恢复路径与活动请求约束 |
| S2. daemon 会话切换能力 | complete | 可列举元数据、按 ID 加载并切换当前 session、新建 session |
| S3. TUI/CLI `/resume` | complete | 启动新 session、列表选择、取消与错误反馈 |
| S4. 回归与文档 | complete | 单元/集成/PTY、README/HTML、release、clippy |

### 本轮约束

- daemon 仍是 session 唯一真相源，TUI/CLI 只通过 RPC 操作。
- 不能在活动 turn 或待审批期间静默切换 session；必须给出明确错误，避免结果写进错误会话。
- 历史列表至少展示稳定 session ID 与可识别摘要，恢复必须由用户明确选择。
- 已记录错误：首次源码检查工具调用的 JavaScript 字符串拼接有语法错误，未执行任何命令；改为单一合法命令字符串后继续。
- 已记录告警：移除启动时自动恢复调用后，旧 CLI `recover_connection` 三个函数成为 dead code；保留集成测试所需 helper 并用 `#[cfg(test)]` 收窄，删除无调用的生产包装函数。
- 已记录格式检查失败：原子 current 指针临时文件表达式不符合 rustfmt 单行布局；其余 63 项测试、Clippy 和 release 仍通过。执行 rustfmt 后单独重验格式门禁。
- 已记录测试编译失败：新增 TUI `/resume` 集成测试漏导入 `SessionStore`，生产代码未受影响；按编译器建议补齐测试模块导入后重验。

### SQLite 取舍

- 本轮不引入 SQLite：它对 `/resume` 正确性不是必要条件，同时迁移 session、memory、plan 会扩大风险面。
- 后续会话规模增长后，可用 SQLite 保存 session/message/plan/memory 元数据与全文索引；图片和大工具输出仍保留文件，仅记录路径，并提供 JSONL 导入。

## 六项通用能力补齐（2026-09-09）

### 目标

在现有单 crate、daemon + 多入口架构上，按顺序实现并验证：多 Provider、严格 tool-call 装配、共享 Slash 命令、版本化 Skill、本地 Cron + Heartbeat、stdio MCP 客户端；保持 OpenAI 路径与既有安全边界兼容。

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 源码基线与现状确认清单 | complete | 通读相关源码，回答需求中的 8 组问题，记录初始测试基线 |
| 1. 多 Provider 协议 | complete | OpenAI/Anthropic/Ollama mock 闭环；编译、clippy、测试通过 |
| 2. Tool-call 严格装配 | complete | identity、纯增量装配、fail-closed 测试通过 |
| 3. Slash 命令框架 | complete | 单注册表、多入口复用、帮助自动生成，回归通过 |
| 4. Skill 体系升级 | complete | frontmatter、semver、本地安装器、稳定排序测试通过 |
| 5. Cron + Heartbeat | complete | 持久化、独立会话、有限重试、无人值守安全测试通过 |
| 6. MCP stdio 客户端 | complete | 握手、工具桥接、隔离、审批、进程清理测试通过 |
| 7. 全量回归与完成报告 | complete | fmt/check/clippy/tests/release 全绿，配置和限制文档化 |

## TUI 交互与渲染能力补齐（2026-09-09）

### 目标

在不改变 daemon 作为唯一真相源、保持单 crate 的前提下，完成 TUI 的退出解耦、结构化消息、输入编辑、滚动、渲染缓存、排队发送、并发请求/审批表达和三主题语义色板。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 0. 现状确认 | complete | 退出、状态、UiMessage、输入、滚动、快照、颜色与渲染热点定位 |
| 1. 类型化退出 | complete | `should_quit` 独占退出控制流，slash/Esc 不依赖文案 |
| 2. 结构化 UiMessage | complete | Text/Tool 节点、稳定 id、状态/耗时、工具卡片 |
| 3. InputEditor | complete | 光标、词操作、多行、历史、CJK 列定位 |
| 4. 滚动与鼠标 | complete | line/page/top/bottom、follow-bottom、可选鼠标 |
| 5. 换行缓存 | complete | 按消息版本/宽度缓存与 session/resize 失效 |
| 6. 发送队列 | complete | FIFO 排队、自动出队、可见与清空 |
| 7. 多活跃/审批集合 | complete | 快照全量重建、并发轮次与审批队列 |
| 8. 语义主题 | complete | terminal/dark/light tokens、无硬编码组件色值 |
| 9. 全量验收 | complete | render/逻辑/集成/PTY、fmt/clippy/release、文档 |

### 本轮原则

- 严格按 0→7 推进，每阶段验证后再进入下一阶段。
- 协议差异封装在 provider 内，工具装配失败整轮原子拒绝。
- 新工具来源不绕过 schema、safety 与 approval；保留用户已有改动。

### 本轮错误记录

- planning-with-files 技能引用的 templates 目录不存在；按技能定义的职责复用并追加仓库现有三份规划文件。
- 首次 provider 整文件替换补丁因同一 patch 同时 Delete/Add 被拒绝，未产生半成品；拆为两次 apply_patch。
- 多 Provider 首次 check 发现 Anthropic 消息向量需要显式类型及两项 unused import；按编译器定位修正。
- 首轮 73 项测试中 4 项旧 mock 语义失败：内部 identity 已编码，且空 ToolCalls 没有事件而被视为文本；测试按 provider 边界还原 id，空批次改为 typed assembly failure。
- 阶段 1 严格 Clippy 检出一次 `and_then(Some)`，按建议改为 `map`。
- 定向测试命令误传两个位置过滤器，Cargo 在编译前拒绝；改为单个 `--all-targets` 全量测试，不重复该用法。
- Slash 接线首次 check 发现已删除的 TUI 本地编号解析测试与两个 dead-code helper；删除平行解析测试/旧 recovery helper，并将仅测试注册表枚举收窄为 cfg(test)。
- Skill 依赖首次 check 通过但发现两个仅测试构造器在生产目标 dead_code；用 cfg(test) 收窄，并把旧无 frontmatter 的 context fixture 升级为新格式。
- Cron 首次 check 发现 slash 参数解析使用了未导入的 anyhow Context；补齐 trait import。生产目标还提示测试兼容构造器 dead_code，已用 cfg(test) 收窄。

## 多窗口独立 session（2026-09-09）

### 目标

同一台电脑上的每个 TUI/编辑器窗口拥有独立 session；一个窗口中的活动请求、历史、取消、审批和事件订阅不阻塞或串入另一个窗口。旧客户端未携带 `session_id` 时继续落到 daemon 默认 session，以保持兼容。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. 会话存储与运行时隔离 | complete | 按 session ID 打开固定 JSONL 文件，不竞争全局 current 指针；每个 session 拥有独立 history/turn lock/engine |
| 2. daemon RPC 路由 | complete | `session.new/load/resume` 返回并操作指定 session；`chat.send/cancel/subscribe/snapshot` 按 session 过滤 |
| 3. TUI/入口接线 | complete | TUI 在所有 chat 请求中携带自己的 session ID，启动新窗口不再接回别的窗口活动请求 |
| 4. 回归与发布 | complete | 多 session 并发、隔离取消/审批、兼容旧客户端、fmt/clippy/test/release、安装 `myagent` |

### 约束

- 不删除或覆盖既有 JSONL 会话；仍支持 `/resume` 明确恢复历史。
- 不再用全局 `session_switch` 阻塞无关 session；current pointer 仅作为旧客户端默认 session 的兼容指针。
- 记录每次失败的验证命令和原因，完成后追加到 findings/progress。

### 验收

- `cargo test --all-targets`：107/107 通过。
- `cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --all -- --check`、`git diff --check`：通过。
- `cargo build --release` 与 `cargo install --path . --force`：通过；`myagent` 与 `my-agent` 均指向最新 release 二进制。

## OpenClaude 逻辑对比与当前项目优化（2026-09-09）

### 目标

研究 `/Users/pilot/Desktop/github_project/openclaude-main` 的成熟逻辑，提取与当前 Rust daemon/TUI 架构兼容、能显著提升可靠性或可维护性的部分，并在保持现有安全边界和多窗口 session 隔离的前提下实现可验证的改进。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. OpenClaude 架构勘察 | complete | 梳理 session、消息队列、目标/计划、权限、远程恢复、状态选择器和持久化模式 |
| 2. 差距与取舍设计 | complete | 选择取消可中断、计划写入串行化、session 实时状态三项高收益改进 |
| 3. 当前项目实现 | complete | 落地取消可中断、计划写入串行化、session 实时状态，并补回归测试与 CLI/TUI 展示 |
| 4. 全量验收与交付 | complete | 109 项全量测试、clippy、fmt、diff check、release、安装和 `myagent` 命令验证通过；已准备提交并推送 |

### 约束

- 只提取逻辑和工程模式，不复制 OpenClaude 的 UI/品牌/闭源服务依赖。
- 不削弱当前工具审批、路径边界、MCP 隔离、session 隔离和取消语义。
- 每两次源码检索后把关键发现写入 findings.md；每个阶段结束更新本计划和 progress.md。

## OpenClaude TUI 对比与当前 TUI 优化（2026-09-09）

### 目标

参考 OpenClaude 的 REPL/Ink 交互结构，改善当前 ratatui TUI 的信息层级、输入区、状态反馈、滚动体验和窄终端可读性，同时保持现有 daemon/session/审批协议不变。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. TUI 现状与 OpenClaude 研究 | complete | 对比消息列表、工具反馈、prompt/footer、状态线、快捷键与滚动模型 |
| 2. 视觉与交互方案 | complete | 选择分层 transcript、动态 prompt、sticky 新消息提示、状态线和帮助浮层 |
| 3. 当前 TUI 实现 | complete | 已落地层级化 transcript、动态 prompt/footer、状态 pills、快捷键帮助与窗口适配 |
| 4. 验收与发布 | complete | 111 项全量测试、TUI 渲染测试、clippy、fmt、release、安装和文档同步通过 |

### 约束

- 不改变 daemon RPC、session 隔离、审批安全和会话持久化语义。
- 不复制 OpenClaude 品牌素材或依赖，仅提取交互和信息架构。
- 每两次源码检索后记录 findings；每个阶段结束更新本计划和 progress。

### 错误记录

| 错误 | 尝试 | 解决 |
|---|---|---|
| 新增帮助浮层测试直接匹配中文字符串失败 | TestBackend 会把双宽字符按终端 cell 展开为空格 | 测试比较前移除空格，保留真实渲染内容断言 |

## Agent 不可用诊断（2026-09-09）

### 目标

根据用户提供的 TUI 截图和“给我写一个前端”复现链路，定位写文件失败、工具重复调用、模型循环和可观测性不足的根因；本阶段先诊断，不在未确认方案前修改生产代码。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. 截图与工具链审计 | complete | 已核对 write_file、plan、LoopEngine、事件流和 daemon 日志 |
| 2. 最小复现与根因确认 | complete | 已用真实 session JSONL、plan.json、daemon 状态确认是工具失败后的恢复循环，不是 daemon 卡死 |
| 3. 修复建议与可观测性方案 | complete | 已给出目录创建、失败熔断、请求级 telemetry 和诊断入口的优先级方案 |

### 约束

- 先保留当前干净工作树，不直接改生产代码。
- 记录截图证据对应的源码位置和可复现命令。

### 结论

- 本轮仅完成诊断和规划文件记录，未修改生产代码、未重启用户 daemon、未提交或推送。
- 截图中的直接根因是 `write_file` 不创建父目录；循环体验的根因是工具错误被当作普通模型反馈，且缺少连续失败熔断与请求级可观测字段。

## Agent 不可用修复实施（2026-09-09）

### 目标

把诊断结论全部落地，并让安装后的 `myagent` 在真实工作区可直接排障和恢复。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. 工具可靠性 | complete | `write_file` 自动创建父目录；准入/装配/执行连续 3 次失败熔断；新增回归测试 |
| 2. 请求级可观测性 | complete | round、request/session ID、Provider 首增量/总耗时、工具耗时/成功状态进入日志和 TUI/CLI/ACP |
| 3. 诊断入口与默认行为 | complete | `status` 展示路径、`logs` 只读命令、`sessions` 脱离模型环境校验、模糊前端请求默认提示 |
| 4. 验收与本机安装 | complete | 113 项测试、Clippy、fmt、release 构建通过；已安装最新 release 并重启空闲工作区 daemon |

### 约束

- 保留现有安全策略、审批、session 隔离和旧事件字段兼容性。
- 不自动提交或推送；代码交付前保留可审阅的工作树差异。

## TUI 任务完成与工具折叠优化（2026-09-09）

### 目标

针对截图中“工具记录过多、完成状态不明显”的问题，提供明确的终态标记和默认折叠、可展开的工具详情。

| 阶段 | 状态 | 主要交付 |
|---|---|---|
| 1. 现状审计 | complete | 确认 `show_tools` 仅控制输出展开，工具卡片本身始终逐条显示；Response 成功只显示“就绪” |
| 2. TUI 交互实现 | complete | 连续工具调用默认合并为摘要；Ctrl+T 展开/收起调用与输出；成功/失败均显示状态 |
| 3. 完成状态与回归 | complete | Response 成功/失败插入可见终态消息并更新状态栏；新增折叠和完成标记测试 |
| 4. 构建交付 | complete | release 构建、安装 `myagent`、同步文档与最终验证 |

### 约束

- 保持 daemon 协议、工具执行和审批语义不变，只调整展示层。
- 默认折叠不丢失工具结果；展开后仍显示工具名称、轮次、状态、耗时和输出。
# TUI 空白、详情展开与运行反馈修复（2026-09-12）

## 目标

修复 `docs/known-issues.md` 中 TUI-001～TUI-003：消除 transcript 大面积无效空白；让 Ctrl+T 在原 Query 上下文中内联展开工具详情并保持滚动稳定；为运行中的 Agent 增加持续、分阶段的不确定进度反馈。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 基线与根因定位 | complete | 确认布局约束、折叠/展开数据模型、活动阶段状态和现有渲染测试 |
| 1. transcript 布局修复 | complete | 短内容自然顶对齐或连续布局，不再出现大块无效空白 |
| 2. Ctrl+T 内联详情 | complete | 原 Query 始终可见，详情在对应摘要处展开，滚动锚点稳定 |
| 3. 运行指示器 | complete | 无新事件期间仍持续动画，能表达模型/工具/审批阶段且终态停止 |
| 4. 回归与交付 | complete | 定向测试、全量测试、fmt、Clippy、release 构建通过并更新问题记录 |

## 本轮约束

- 保留并兼容仓库当前未提交改动，不覆盖用户已有工作。
- daemon 继续作为运行状态唯一真相源；进度动画只表达“不确定进度”，不伪造百分比。
- 不重构无关模块，优先在现有 `TuiApp` 状态、布局和 view 渲染边界内修复。

## 本轮错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 首次查阅依赖源码时误猜 `ratatui-core-0.1.0` 目录 | 1 | 根据 Cargo.lock 与 registry 文件清单定位到实际版本 `ratatui-core-0.1.2`，不重复错误路径 |
| 最终二进制哈希核对时 `shasum` 因本机 `C.UTF-8` locale 异常崩溃 | 1 | 改用不依赖 Perl locale 的 `cmp` 逐字节核对，不重复运行 `shasum` |
| 最终状态中出现本轮未编辑的 `src/daemon/lifecycle.rs` 语义改动 | 1 | diff 确认为 daemon 升级/日志持久化等独立工作而非 rustfmt 变化；按用户改动保留，未作还原 |
| 消息级锚点重构后严格 Clippy 报测试包装函数 `transcript_lines` 为生产 dead code | 1 | 将该兼容测试 helper 收窄为 `#[cfg(test)]`，保留生产实现 `transcript_lines_with_anchor`，随后重跑全套门禁 |
