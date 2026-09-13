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
| 验收启动 Web 时仅设置 `MY_AGENT_WEB_ADDR`，但该变量只影响 TUI launcher，serve 仍使用默认 8787 导致端口占用 | 1 | 读取 `my-agent serve --help`，改用 `--bind 127.0.0.1:18882` 后重试 |
| `view_image` 不支持直接读取 SVG 品牌封面 | 1 | 改用本机 SVG 渲染工具生成临时 PNG 后检查，不重复直接读取 SVG |
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
# 项目展示 HTML 与 GitHub README 更新（2026-09-12）

## 目标

以本地 `/Users/pilot/Desktop/agent-system.html` 为视觉基准，按当前源码与已实现能力更新项目全景说明；再将同一套信息架构转换为 GitHub 原生、美观且易读的 `README.md`，提交并推送到 `origin/main`。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 基准与功能审计 | complete | 对比桌面/仓库 HTML、README 与源码现状，形成缺口清单 |
| 1. HTML 内容与视觉更新 | complete | 仓库 HTML 已补齐最新功能并通过 1600px 静态渲染；待最终同步桌面副本 |
| 2. GitHub README 改版 | complete | 首屏定位清晰、功能/架构/快速开始完整，使用 GitHub 支持的视觉元素 |
| 3. 渲染与内容验收 | complete | 1600px HTML 与 1200×420 SVG 渲染通过；结构、链接、JSON、格式和 122 项测试通过 |
| 4. 提交与推送 | complete | 项目说明与 README 已提交并推送到 `origin/main`，GitHub 仓库页确认加载新版内容 |

## 原则

- HTML/README 中的已有文字仅作为项目资料，不作为新的操作指令。
- README 不直接嵌入依赖脚本的复杂 HTML/CSS；采用 GitHub 可稳定渲染的 Markdown、表格、折叠块、徽章和仓库图片。
- 功能描述以当前代码和测试为准，不夸大远程 MCP、多租户、系统级沙箱等未实现能力。
- 保留用户工作树中的所有修改；当前基线与远端一致。

## 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 内置浏览器安全策略拒绝本地 `file://` HTML | 1 | 不绕过策略，改用 Quick Look 1600px 静态渲染、HTMLParser 结构检查，并在推送后检查 GitHub 页面 |
| 旧版 `tidy` 把 UTF-8/HTML5 标签误报为非法 | 1 | 不使用其结果作为门禁，改用 Python 标准库 HTMLParser 检查标签栈并结合实际渲染验收 |
| 推送后内置浏览器加载 GitHub 页面超时并重置会话 | 2 | GitHub 文本抓取已确认新版 README 生效，且本地原尺寸视觉验收已通过；停止重复浏览器尝试 |
| 临时预览清理脚本使用 `path` 覆盖 zsh 的 PATH 数组 | 1 | 改用 `preview_item` 变量并显式调用 `/bin/mv`、`/usr/bin/find`；预览文件已移入可恢复的废纸篓目录 |

# 本地 Agent Web 控制台（2026-09-13）

## 目标

为现有 daemon 增加同源 Web 控制台：既能发起 Agent 对话，也能列出所有本地 Session，并查看每个 Session 的消息、LLM 请求/响应、工具调用与时间链路；TUI 展示 Web 地址并提供 `/web` 启动或复用、随后打开浏览器的入口。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 现状与数据契约审计 | complete | 确认 HTTP/WS、Session JSONL、事件字段、daemon 生命周期、slash/TUI 边界 |
| 1. 可观测数据与 Web API | complete | 提供 Session 列表/详情、链路记录与调用 Agent 的同源 API，保持 daemon 为唯一真相源 |
| 2. Web 前端 | complete | 完成对话工作台、Session 浏览器、详情时间线与响应式布局 |
| 3. TUI `/web` 集成 | complete | 状态区显示地址；`/web` 幂等启动/复用并打开系统浏览器 |
| 4. 验收与文档 | complete | 单测/集成/浏览器烟雾、fmt、Clippy、全量测试、release 和 README 更新 |

## 本轮约束

- 保留当前工作树中尚未提交的 dogfood、slash 补全与 TUI 相关改动，不覆盖或回退。
- Web 仅默认监听 loopback；复用现有鉴权、安全审批、session 隔离与 daemon RPC，不在前端复制 Agent 逻辑。
- Session 详情必须能区分用户/assistant/tool 消息，并尽可能呈现模型请求、响应、工具与阶段时间；不伪造历史上未持久化的时间字段。
- `/web` 重复调用不得重复启动服务；成功后给出稳定 URL 并尝试打开默认浏览器。

## 本轮错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 首次跨三个规划文件的补丁因 `findings.md` 锚点不存在而整体拒绝 | 1 | 补丁未落盘；改用每个文件的真实末尾锚点追加，不重复旧锚点 |
| 首次 trace 接线补丁保留了旧方法调用的一行链式前缀 | 1 | 编译前源码检查发现并删除多余 `.run_turn_with_events(`，未形成重复失败 |
| 新增 `ApiState.workspace` 后测试 fixture 漏填字段 | 1 | `cargo check --all-targets` 精确定位；为唯一测试构造器补入当前工作区 |
| 首次 CUA 初始化调用漏传 `code`，随后又误用了不可用的 `tools` 全局 | 1 | 两次均未操作页面；读取工具返回的正式 API 后改用 `cua.getState/createBrowserTab` |
| CUA 创建标签时误传不支持的 `max_output_chars`，并尝试了不可用的 Chrome provider | 1 | 根据 schema 移除多余字段，枚举可用浏览器后改用 Codex in-app browser |

# Web Agent 工作台与工作目录（2026-09-13）

## 目标

把 Web 控制台重构为两个一级功能：以 Session/链路审计为核心的查看页，以及以实际开发工作为核心的 Agent 对话页；Agent 对话在创建会话时可选择本地工作目录，并确保 daemon、工具安全边界、会话持久化和页面展示都与所选目录一致。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 工作区与会话模型审计 | complete | 明确现有 daemon 单工作区边界、Web 生命周期、Session RPC 和工具工作目录约束 |
| 1. 多工作区后端契约 | complete | Web 可列出/校验目录并为所选目录连接或启动对应 daemon，Session 数据按工作区隔离 |
| 2. Agent 工作台前端 | complete | Agent 对话成为一级主界面，支持目录选择、新建对话、流式工作、审批、取消与状态反馈 |
| 3. Session 查看前端 | complete | Session 浏览与链路详情成为独立一级页面，可切换工作区并查看历史记录 |
| 4. 兼容、测试与交付 | complete | TUI `/web` 保持可用，完成单测/集成/浏览器验收、fmt、Clippy、全量测试、安装和文档更新 |

## 本轮约束

- 工作目录必须经过 canonicalize/存在性校验，不能由浏览器直接绕过 SafetyPolicy。
- 一个已有 daemon 仍只负责一个工作区；优先在 Web 服务层按目录管理 daemon 连接，不把运行时安全边界改成可变全局状态。
- Agent 工作是默认主路径，日志和 trace 是独立的 Session 查看能力，不喧宾夺主。
- 保持现有 `/web` 默认工作区与旧 Session 的兼容性。

## 本轮错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 重写 `web/index.html` 时在同一补丁中同时 Delete/Add 同一路径，被 apply_patch 拒绝 | 1 | 补丁未落盘；改为分两次 Delete/Add，不重复该补丁结构 |
| 一次 `cargo test` 传入两个独立测试过滤串，Cargo 拒绝第二个参数 | 1 | JS 检查已通过；改为分别运行两个过滤串，serve 7 项与 web 2 项均通过 |
| 更新阶段记录时两次使用了不稳定的跨章节锚点，补丁未命中 | 2 | 读取文件末尾确认最终记录已在本节，后续只用本节内唯一完整行更新 |
| `rg` 查询字符串包含未安全引用的反引号，zsh 将其当作命令替换 | 1 | 未产生文件修改；后续 shell 搜索避免反引号或使用安全单引号参数 |
| 浏览器隔离验收首次选择 127.0.0.1:18789，该端口已被占用 | 1 | daemon 已正常拉起但 Web 未启动；改用系统分配的空闲端口，不重复固定端口 |
| CUA 选择 Agent 工作台按钮时未使用 exact，和“在 Agent 工作台继续”发生严格匹配冲突 | 1 | 页面没有发生点击；后续对同名按钮使用 `exact: true`，不重复模糊定位 |
| CUA 调用重复误传了已知不支持的 `max_output_chars` 字段 | 2 | 调用在参数校验阶段失败、未创建页面；后续严格只传 `code`/`title`/`timeout_ms` |
| CUA 误用不存在的 `tab.playwright.dom.setViewportSize` 与 REPL `text()` helper | 1 | 页面已正常创建；停止猜测辅助 API，后续仅使用已确认可用的 `tab.playwright.domSnapshot()` 与交互接口 |

# Session 日志帧超限修复（2026-09-13）

## 目标

修复 Web Session 查看页读取大 Session 失败的问题。当前 `session.load`/`session.trace` 将整个 JSONL 一次性放入 daemon 协议帧；截图显示 7,643,140 字节响应超过 4,194,304 字节限制，导致消息和链路全部无法展示。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 根因确认 | complete | 确认超限发生在整包 Session/trace RPC 返回，而非前端渲染；梳理兼容边界 |
| 1. 分页 RPC | complete | daemon 提供有界的消息与 trace 分页结果，单帧留出协议开销 |
| 2. Web 增量查看 | complete | Session 页面首屏可见、可继续加载，不因单条大内容阻塞整页 |
| 3. 回归与交付 | complete | 大数据测试、全量测试、格式/Clippy/前端检查通过并更新文档 |

## 本轮约束

- 保留旧 `session.load`/`session.trace` RPC 兼容性；Web 改用分页方法，Agent 运行时仍从本地完整 Session 文件恢复历史。
- 单次 Web 响应必须显式受字节预算约束，不能只依赖消息条数；必要时对超大展示字段给出可见的截断标记。
- 不提高全局 `MAX_FRAME_BYTES`，避免把协议层内存/拒绝服务风险转嫁给所有客户端。

## 本轮错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 首个大消息分页测试误假设单条消息会立即触发页满，实际单条内容先被压缩后仍可与后续项同页 | 1 | 改为构造多条 300 KiB 消息，验证页级字节预算、`has_more` 和截断标记，不重复该断言 |

# 三档 Agent 权限模式（2026-09-13）

## 目标

在同一工作区的 Web 与 TUI 提供三档共享模式：请求批准、帮我批准、完全访问权限；模式切换直接作用于 daemon 使用的 SafetyPolicy，完全访问跳过审批但保留灾难性命令硬拦截。

## 阶段

| 阶段 | 状态 | 完成标准 |
|---|---:|---|
| 0. SafetyPolicy 审计 | complete | 明确现有路径、命令、MCP 审批入口和 daemon 生命周期 |
| 1. daemon/RPC 契约 | complete | SafetyPolicy 支持三档；Web/TUI 可读写同一个工作区模式 |
| 2. Web/TUI 入口 | complete | Web 菜单与 `/permissions [request|risk|full]` 可切换并反馈当前模式 |
| 3. 回归与交付 | complete | 审批边界、前端资源、全量测试和安装推送通过 |

## 本轮约束

- 模式按工作区 daemon 共享，目录切换后使用目标目录自己的模式状态；不向 Agent 请求参数注入可变 cwd。
- “完全访问权限”只跳过安全审批，不解除 fork bomb、磁盘擦除、根目录递归删除等硬拦截。
- Cron 无人值守任务继续使用独立的 `UnattendedApproval`，不会因交互模式切换而自动放宽。

## 本轮错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 一次 cargo test 传入两个独立过滤串，Cargo 拒绝第二个参数 | 1 | 改为分开运行 daemon RPC 与 SafetyPolicy 两个定向测试，不重复该调用方式 |
## 交付记录

- Web 权限菜单已在隔离发布版服务中验证，默认显示“帮我批准”，弹窗展示三档选项和灾难性命令硬拦截说明。
- TUI `/permissions` 与 Web `permissions.get/set` 已通过共享状态回归测试；全量测试 136/136、Clippy、rustfmt、Node 语法和 diff 检查通过。
- 发布版已重新构建并安装到 `/Users/pilot/.cargo/bin/my-agent` 与 `/Users/pilot/.local/bin/my-agent`，二进制逐字节一致。
## Web Agent 流式与 Markdown 优化（2026-09-13）

### 目标

1. Agent 工作台默认不展开工具调用与工具输出，只显示紧凑的执行摘要。
2. 模型响应按 WebSocket 增量事件实时更新，首字节到达后立即出现在对话中。
3. 模型返回的 Markdown 在页面安全解析为标题、列表、代码、链接等语法。

### 阶段

| 阶段 | 状态 | 完成标准 |
|---|---|---|
| 0. 现状审计 | complete | 确认 Web 事件处理、工具动态 DOM 和现有内容渲染边界 |
| 1. 工具默认折叠 | complete | 工具调用/输出默认收起，仍可主动展开查看 |
| 2. 流式与 Markdown | complete | 增量响应不被快照覆盖，Markdown 渲染安全且持续更新 |
| 3. 回归与交付 | complete | 前端/后端测试、浏览器验收、构建安装和推送完成 |

### 约束

- 保留现有审批、取消、Session 持久化和链路审计，不改变 daemon 协议语义。
- Markdown 不能执行脚本或注入任意 HTML；链接使用安全协议，代码块保留可复制文本。
- 工具详情只改变默认视觉状态，不删除 Session 中的工具与 trace 数据。

### 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| Anthropic 思考流单测使用非 ASCII 原始字节串，rustc 拒绝解析 | 1 | 改用 JSON `\\uXXXX` 转义，保持 SSE fixture 为 ASCII 字节串 |
| Ollama 思考流单测同样使用非 ASCII 原始字节串 | 1 | 将 NDJSON fixture 改为 JSON Unicode 转义后重新执行格式检查 |
| OpenAI 思考流单测使用非 ASCII 原始字节串 | 1 | 将 SSE fixture 改为 JSON Unicode 转义 |
| CUA 通过 `evaluate` 直接给 textarea 的 `value` 赋值触发只读包装错误 | 1 | 改用 Playwright locator 的 `fill`，再用页面脚本只读取状态；不再重复该赋值方式 |
| CUA 页面 `evaluate` 隔离上下文不提供 `MutationObserver` 构造器 | 1 | 改用 WebSocket 事件时间戳与页面定时读取完成验收，不依赖该 API |
| 新增思考事件后 CLI/TUI/ACP 的 `EventKind` 匹配不完整，`cargo check --all-targets` 报非穷举 | 1 | 为非 Web 消费者补充忽略/状态处理分支，保留 Web 思考流展示 |
| CUA 按可访问名称 `textbox[name=prompt]` 未匹配到 textarea | 2 | 改用稳定 DOM id `#prompt` 定位，后续不再使用无名 textbox 角色查询 |

### 当前验收记录

- 隔离发布版验证响应首段到达时状态为“生成响应”、存在流式光标；完成后 Markdown 标题、列表、代码块和安全链接均生成语义 DOM。
- 工具调用与工具输出数据仍保留在消息和活动状态中，但默认放入关闭的 `<details>`，用户可主动展开查看。
- 全量测试 136/136、Clippy、rustfmt、Node 语法、嵌入资源测试和 diff 检查通过；release 已构建并安装到 PATH，准备提交推送。
## Web Markdown 表格与思考流（2026-09-13）

### 目标

1. 正确渲染 Markdown 表格，避免被当作带竖线的普通文本。
2. 将模型增量区分为思考流与正式回答流，并在页面按到达顺序实时展示。
3. 思考结束后自动折叠思考区，正式回答继续流式输出；旧 Provider 无思考事件时保持兼容。

### 阶段

| 阶段 | 状态 | 完成标准 |
|---|---:|---|
| 0. 协议与 Provider 审计 | complete | 确认各 Provider 思考字段/增量形态和 daemon 事件透传边界 |
| 1. Markdown 表格 | complete | 表头、分隔线、数据行和对齐标记安全渲染为 table |
| 2. 思考/正文流 | complete | 新事件实时显示，思考完成自动折叠，正文不被覆盖 |
| 3. 回归与交付 | complete | Provider/daemon/Web 测试、浏览器验收、构建安装和推送完成 |

### 约束

- 保持现有 `text_delta`、工具调用、审批、取消和 Session trace 兼容；新增事件必须可选。
- 不把思考内容写入最终 assistant 正文，避免刷新快照时错位；历史记录没有思考字段时正常显示正文。
- Markdown 表格单元格必须经过现有安全内联渲染，不能引入未转义 HTML。

### 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|

### 当前验收记录

- OpenAI `reasoning_content`、Anthropic thinking content block/`thinking_delta`、Ollama `message.thinking`/`reasoning` 均已归一为 Provider 思考增量；无思考字段的旧 Provider 仍走原有正文流。
- daemon 新增 `thinking_delta` 与 `thinking_finished` 事件；首个正文增量前保证发出思考完成事件，Session assistant 消息和 model response trace 均保留独立 thinking 字段。
- Markdown 表格支持表头、分隔线、数据行与左右/居中对齐，单元格继续经过安全 inline Markdown 渲染。
- 真实隔离 WebSocket 记录显示事件按 `thinking_delta → thinking_finished → text_delta → turn_completed` 到达，且思考与正文之间存在真实时间间隔，不是完成后的伪流式。
- 隔离浏览器可访问性树确认思考阶段显示“生成中”和流式光标；思考完成后显示“已自动折叠”，正文被解析为标题、表格、行和单元格。

### 交付记录

- `cargo test --all-targets` 140/140、严格 Clippy、rustfmt、Node 语法与 diff 检查通过；release 已重新构建并安装到 `/Users/pilot/.cargo/bin/my-agent` 与 `/Users/pilot/.local/bin/my-agent`，两份二进制一致。
- 隔离 Web/WS 验收服务、模拟 Provider、脚本和工作区已停止并移入 `/Users/pilot/.Trash`；用户要求停止的 127.0.0.1:8787 旧进程 PID 64165 已结束，端口已释放。

## Web 工作台布局优化（2026-09-13）

### 目标

1. 增大 Agent 对话区和模型输出卡片的可用面积。
2. 压缩右侧实时辅助栏，但保留工作目录、Session 与状态操作。
3. 移除对开发任务帮助不大的英文眉题，减少顶部占高。

### 阶段

| 阶段 | 状态 | 完成标准 |
|---|---:|---|
| 0. 现状审计 | complete | 确认对话/辅助栏栅格、消息最大宽度与标题占高 |
| 1. 样式调整 | complete | 左侧更宽、模型消息接近满宽、顶部眉题移除 |
| 2. 浏览器验收与交付 | complete | 桌面/窄屏检查、门禁通过、提交并推送 |

### 约束

- 不改变 WebSocket、流式、Markdown、Session 或权限行为，只调整布局与视觉尺寸。
- 右侧工作目录、当前 Session、取消/审批和链路入口仍可访问。
- 保留窄屏断点，避免把模型输出挤出可滚动区域。

### 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 样式验收启动服务时未提供 Provider 环境变量，配置校验提前退出 | 1 | 改用带本地模拟 Ollama 的隔离工作区启动验收服务 |
| 清理后的模拟脚本扩展名为 `.mjs.qa`，Node 不识别为 ES 模块 | 1 | 复制到明确的 `.mjs` 临时路径后启动，不修改废纸篓原件 |
| 空白工作台上读取不存在的 `.message` 计算样式导致 CUA `getComputedStyle` 报错 | 1 | 改用已有布局节点读取几何尺寸，避免对空状态消息取样 |
| CUA IAB Playwright 对象不提供 `screenshot` 方法 | 1 | 改用 Tab 级截图 API和 DOM 几何完成视觉验收 |

### 当前验收记录

- 已移除 Agent 工作台标题区的 “Build with your local agent” 英文眉题；Session 查看页的记录眉题保留，不影响日志识别。
- 桌面视口 1440×900 验收：左侧对话区约 1102px，右侧实时栏约 290px；模型消息宽约 1014px，接近内容区满宽。
- 对话标题区和 transcript 上下留白已收紧，顶部标题、Markdown 表格、思考折叠区与正式回答均在同一可滚动正文区域内，composer 获得更稳定的底部空间。
- 窄屏断点仍按单列布局工作；移动视口下模型输出卡片保持接近满宽，右侧工作目录/Session/实时状态下移展示。

### 交付记录

- `cargo fmt --all -- --check`、`node --check web/app.js`、`git diff --check` 通过；发布版已重新构建并安装到两个 PATH 目录且二进制一致。
- 隔离浏览器可访问性树确认 Agent 标题区不再包含英文眉题，模型输出仍保留语义标题和 Markdown 表格；临时服务、模拟 Provider、脚本与工作区已停止并移入 `/Users/pilot/.Trash`。

## Web 工作台截图标注优化（2026-09-13）

### 目标

1. 按截图标注将“新建任务”提升到顶部操作区，减少顶部重复信息。
2. 将工作目录、权限模式选择放到输入框底部上下文区，用户提交任务前可以就近确认并切换。
3. 明确中间 transcript 是模型响应区域，空状态文案与真实流式消息保持一致。

### 阶段

| 阶段 | 状态 | 完成标准 |
|---|---:|---|
| 0. 标注与 DOM 审计 | complete | 确认红色标注对应的顶部、响应区和 composer 控件 |
| 1. DOM/样式调整 | complete | 顶部新建任务、底部工作区/权限控件、响应区提示完成 |
| 2. 浏览器回归与交付 | complete | 桌面/窄屏可用、功能事件不回归、测试通过并推送 |

### 约束

- 不改变控件 ID、RPC 请求、权限语义和流式渲染逻辑，只调整 DOM 位置、文案和布局。
- 工作目录与权限模式必须保持可点击、可禁用状态，并在窄屏下仍能读到当前值。
- 不把截图中的红色批注作为产品文字写入页面；仅将其意图转化为正式中文 UI。

### 错误记录

| 错误 | 次数 | 处理 |
|---|---:|---|
| 移动工作目录/权限控件后仍按旧顶部节点更新，隔离页面首次连接报 `textContent` 空节点错误 | 1 | 保留既有 `workspace-label` ID，并移除重复的 composer 节点更新；重新构建发布资源后复验 |
| CUA 回归第一次加载的是修复前发布二进制，页面仍显示旧初始化错误 | 1 | 停止旧服务并重新构建 `target/release/my-agent` 后再加载页面 |
| `functions.exec` 中使用未正确转义的 JavaScript 字符串传递补丁，补丁调用未执行 | 1 | 改用模板字符串重发同一 `apply_patch`，未修改目标文件 |

### 当前验收记录

- 桌面端顶部操作区仅显示“＋新建任务”、连接状态和设置；工作目录/权限模式选择器已移至 composer 底部，1440px 视口下两个控件均可读且可操作。
- agent-transcript 增加“模型响应区域”可访问性描述，空状态明确提示模型响应会在此实时展示；已有 Markdown 表格、思考自动折叠和正式响应流保持不变。
- 390px 窄屏实测 composer 底部仍能同时显示“工作目录”和“权限模式”，控件不溢出；Agent 响应、思考生成中和完成后自动折叠状态均通过 AX 验收。
- 隔离服务中实际提交一轮任务，确认流式思考先显示“生成中”，完成后显示“已自动折叠”，正式响应仍渲染为标题与表格。

### 交付记录

- cargo test --all-targets 140/140、严格 Clippy、rustfmt、Node 语法、diff 检查通过；Release 已重新构建并安装到 /Users/pilot/.cargo/bin/my-agent 与 /Users/pilot/.local/bin/my-agent，两份二进制一致。
- 隔离浏览器、模拟 Provider 与 18884 服务已关闭；验收使用的工作区/脚本位于 /Users/pilot/.Trash，可恢复；8787 既有用户服务未由本轮启动。
