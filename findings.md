# 实施发现

- 当前目录 `/Users/pilot/Documents/myproject/agent-rust` 为空，且不是 Git 仓库。
- 用户蓝图要求一个 binary crate、约 8 个职责模块、统一 tokio 异步运行时，以及逐阶段 build/clippy 验证。
- 非测试代码不得使用 `unwrap()` / `expect()`；应用边界使用 anyhow，领域错误使用 thiserror。
- 真实 LLM 验证受环境变量与外部服务可用性影响，因此自动化验收需要 mock provider 覆盖工具调用闭环。
- 2026-09-07 探测 `rsproxy.cn` 与 USTC crates.io 镜像均返回 HTTP 200；选用 rsproxy sparse 并限制在项目级配置。
- 阶段一采用 SSE 行增量解析，将分片的文本和工具名称/arguments 分别累积；工具 arguments 完整后再解析 JSON。
- assistant 的工具调用消息与 tool 结果都进入历史，tool 结果通过 `tool_call_id` 严格配对。
- 最终实现保持一个 binary crate 和单进程 CLI；核心由 8 个职责模块构成，阶段五只增加 JSONL 关键词记忆。
- 上下文压缩仅改变内存中的请求视图；session.jsonl 保留原始 append-only 对话，重启后会在需要时重新压缩，兼顾可审计性和实现简单度。
- 工作区外只读直接拒绝，工作区外写入/编辑走审批；该选择避免并行只读波次同时争抢终端审批输入。
- DeepSeek 的 OpenAI 兼容端点 `https://api.deepseek.com/chat/completions` 与 `deepseek-v4-flash` 已在 2026-09-08 实测支持本项目的流式工具调用格式。

## 进阶基线核对与实现结论

- Cargo 仍是单 binary crate；依赖均为开源 crate，Rust 2024 edition，最低 Rust 1.85。
- `main.rs` 确实只提供 CLI REPL，启动时组装安全、6 个工具、上下文、session 和记忆。
- `provider.rs` 的 `Message.content` 目前只能表达纯文本，图片内容块需要扩展消息模型；SSE 已能累积文本和 function arguments 分片。
- provider 已解析 `prompt_tokens_details.cached_tokens` 并用 tracing debug 输出，但当前请求未发送显式 cache_control，工具定义也作为顶层 `tools` 字段而不是上下文消息。
- `loop_engine.rs` 确实实现 50 轮 ReAct、连续只读最多 8 路并行、副作用串行、tool_call_id 回填与逐消息持久化。
- `context.rs` 当前只有 80% 单闸门；稳定消息顺序为系统提示→AGENTS.md，随后历史与动态环境。工具 schema 是 Provider 顶层字段，无法由 ContextManager 直接插在两个系统块之间。
- `safety.rs` 是文件路径与命令的唯一决策点；工作区外读拒绝、外写/编辑审批，灾难命令硬拒、高风险命令审批，与基线一致。
- `session.rs` 是 append-only JSONL，能忽略崩溃残缺末行并用 Tokio Mutex 提供无超时 RAII turn 锁；`memory.rs` 是 TTL JSONL + 关键词/中文 bigram，需模型主动调用。
- 工具注册表当前存 `Box<dyn Tool>` 且不可克隆/筛选。sub_agent 要复用工具实例并创建受限子集，需要改为 `Arc<dyn Tool>` 或增加共享子集视图。
- `LoopEngine` 当前把主 session 持久化与 turn 锁写死在执行入口中。sub_agent 需要抽出不持久化、独立历史、可配最大轮次的内部运行入口。
- `read_file` 对 PNG/JPG/WebP/PDF 等均只返回降级说明；消息 content 只有 String，图片多模态需要向后兼容地扩展内容块表达。
- 基线描述与真实代码总体一致；主要偏差是“稳定前缀中的工具位置”受 OpenAI 顶层字段协议约束，以及缓存 usage 已记录但没有显式断点。
- DeepSeek 官方文档确认上下文缓存对所有用户自动启用、不需要代码或接口变更，只匹配从第 0 token 开始的相同前缀；返回字段为 `prompt_cache_hit_tokens` 与 `prompt_cache_miss_tokens`。因此没有向 OpenAI 兼容请求添加非标准 cache_control，而是稳定排序工具 schema 并补齐两种 usage 形状。
- 图片按 DeepSeek/OpenAI 兼容协议使用 user 消息的 `text` + `image_url` 内容块；工具回执仍保持字符串和 tool_call_id 配对。base64 只存在于当前 turn 的临时消息，不写入 session。
- PDF 采用开源 `lopdf` 在 `spawn_blocking` 中本地抽取，限制 16 MiB、50 页与约 512K 字符；不做视觉渲染。
- 两级压缩默认水位为 60%/85%；温和模式只摘要可压缩旧历史的最老三分之一，强力模式保留最近配置数量。
- skill 从 `.my-agent/skills/*.md` 建立标题/摘要索引，英文关键词与中文 bigram 匹配，最多加载 3 个正文；目录不存在时无感禁用。
- cron 与 MCP 属第三批明确可停项，本轮在完成 skill 后停止扩张，留待独立设计。
- `lopdf` 已关闭默认的 chrono/jiff/rayon/time features；本地 PDF 生成与抽取回归测试仍通过，减少了不必要的依赖面。
- 当前工具执行环境未配置 `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `MODEL_NAME`，故进阶功能以 mock Provider、真实本地文件和 CLI 烟雾测试完成验收；先前基础版本的 DeepSeek 工具闭环实测记录仍保留。

## Daemon 架构基线核对与最终结论

- 附件要求一个工作区对应一个 daemon，CLI/HTTP/编辑器入口全部通过同一 JSON-RPC 方法访问，不允许入口复制业务逻辑。
- 当前 `main.rs` 直接创建 Provider、SafetyPolicy、8 个工具、MemoryStore、PlanStore、ContextManager、SessionStore 与 LoopEngine，并直接持有 `Vec<Message>` 进入阻塞式 stdin REPL；它确实是单进程直连。
- 当前 `LoopEngine` 持有 Provider、ToolRegistry、ContextManager 和可选 SessionStore；主入口用 `Some(session)`，sub_agent 使用 `None` 的 ephemeral 模式。这为 daemon 收拢提供了复用入口，但主历史仍由调用者以 `&mut Vec<Message>` 传入。
- Provider 当前只在完成整个 SSE 后返回 `Response`，尚未向上游发出文本增量或工具开始/结束事件；阶段 A 的“流式 chat.send”需要给共享执行层增加事件 sink，而不是由入口模拟。
- `SessionStore` 的 turn 锁在 `LoopEngine::run_turn` 内部获取；daemon 若同时持有 history 锁与该锁，需要固定锁顺序以避免死锁。
- 现有审批实现是同步终端 `y/N` 回调，daemon 化后必须改为可挂起的审批请求状态，并由 `approval.respond` 唤醒；入口不能直接读 daemon 的 stdin。
- 现有代码尚无取消 token、session 清单元数据、会话 ID、socket 生命周期、HTTP server、clap 子命令或配置对象。
- `PlanStore` 已支持 set/update/add/show、原子临时文件提交和动态上下文注入，适合作为 daemon 持有的共享状态，不应在各入口重复创建。
- `MemoryStore` 与 skill 库均以工作区路径为作用域；daemon 化后应只初始化一次并由所有入口共享。
- `ReadFileTool` 已支持图片临时内容块及 PDF 本地抽取；daemon 事件协议应避免把大型 base64 工具中间内容重复广播给客户端。
- `ToolRegistry` 内部使用 `Arc<dyn Tool>` 且可 clone/subset，适合直接迁入 `DaemonState` 的单实例装配。
- `Cargo.toml` 尚无 `clap`、HTTP server、socket 辅助或哈希依赖；阶段 A 可仅用现有 Tokio `mpsc/oneshot` 完成内存回环，阶段 B/C 再按需引入依赖。
- Provider 的 SSE 解析入口集中在 `parse_sse`/`consume_sse_line`，可用可选 `mpsc::UnboundedSender<ProviderEvent>` 向上转发真实文本分片，同时让现有 mock 仅实现 `chat` 并通过 trait 默认方法保持兼容。
- 阶段 A 采用轻量自研 `CancellationToken(Arc<AtomicBool>)`，在模型请求和工具波次外层用 `tokio::select!` 响应取消，无需新增依赖。
- `SafetyPolicy` 只依赖 `Arc<dyn Approval>`，因此可用 daemon `ApprovalBroker` 无侵入替换 `TerminalApproval`；审批表与活动请求表必须使用独立锁，避免等待审批时阻塞 `approval.respond`/`agent.cancel`。
- 当前 session 仅有一个活跃 JSONL 与轮换备份；阶段 A 的 `session.list` 将如实枚举当前文件及同目录备份，而不虚构多会话数据库。

## 阶段 B 设计结论

- Unix 客户端需要保持一条持久连接并按 request_id 多路复用；若每个 RPC 新建连接，“最后客户端断开退出”会导致 REPL 两轮之间 daemon 被反复拉起。
- socket server 将每个请求交给同一 `DaemonState::handle_request`，连接层只负责 NDJSON 解码/编码和 4 MiB 限制，不复制 RPC 业务逻辑。
- 运行目录采用系统临时目录下 `my-agent/<稳定工作区哈希>`，同时允许 `MY_AGENT_RUNTIME_DIR` 覆盖；目录权限设为 0700，避免 socket/PID/ready 在不同工作区冲突。
- daemon 空闲退出只在“至少连接过客户端、当前客户端为 0、没有活动 turn”同时成立后触发；活动任务绝不由空闲计时器强杀。
- 已抽出 `daemon::runtime::build_daemon_state`，后续 daemon 进程成为 Provider/工具/安全/历史的唯一装配点，CLI 不再装配运行时。

## 阶段 C 与生命周期补充

- HTTP 普通与 SSE 路径都直接消费 `DaemonClient` 的同一事件流；HTTP 无交互审批能力时采用安全默认值“自动拒绝”，避免危险操作被静默放行或请求永久挂起。
- OpenAI 兼容入口只提取请求中最后一条非空 user 文本；对话真相仍来自 daemon session，避免客户端重复上传的全量历史被再次持久化。
- HTTP `serve` 进程持有一条持久 daemon 连接，因此服务存活期间 daemon 不会触发“最后客户端断开”退出。
- 烟雾测试用 PTY Ctrl-C 停止 `serve` 时发现 daemon 残留 pid/ready；根因推断为自动拉起的子进程继承前台进程组并同时收到 SIGINT。修复方向：daemon 子进程独立 process group，且 server 自身监听 Ctrl-C 做清理。
- 上述修复复测后 pid/ready/socket 均正常消失；运行目录只保留 daemon.log，`status` 稳定返回 stopped。
- stdio 编辑器入口采用并发转发：stdin 持续读取请求，按原 request_id 启动独立转发任务，stdout 单写协程串行输出，因此 chat 等待审批时仍能接收 `approval.respond` 或 `agent.cancel`。
- HTTP 失败烟雾测试会先持久化 user 消息再遇到上游错误，这是正确的 Agent 语义但会污染开发工作区；已将本次测试创建的两行 session 文件移入废纸篓，后续烟雾测试需显式设置临时 SESSION_PATH。
- README 与功能总览 HTML 仍描述“单进程、单 CLI、无 daemon/多入口、启动询问恢复”，已经与新实现相反；最终验收必须整体更新架构、请求首段、模块表、运行命令和能力边界。
- 桌面 HTML 的核心工具/记忆/安全内容仍有效，可保留视觉样式，重点重写入口→daemon→ReAct 的前半链路并新增 daemon/client/entry 模块说明。
- 最终形态由一个按工作区隔离、按需自动拉起的 daemon 持有全部运行时真相；CLI、HTTP 与 stdio 编辑器入口都只使用 `DaemonClient` 和统一 JSON-RPC 协议。
- 并发审批必须使用 task-local 请求上下文；全局可变“当前事件出口”会让排队请求覆盖正在等待的审批路由。对应双请求回归测试已固定该约束。
- 最终 48 项测试、release、fmt 与严格 Clippy 全绿；隔离 stdio 实进程测试确认协议响应和 daemon 空闲退出，HTTP/UDS 生命周期测试也已覆盖。

## 标准 ACP + WebSocket + 重连恢复：阶段 0 发现

- daemon 私有 RPC 真实方法为 `chat.send`、`session.load`、`session.list`、`session.new`、`approval.respond`、`agent.cancel`、`daemon.stop`。
- `chat.send` 参数是 `{message: String}`；`approval.respond` 是 `{approval_id: String, approved: bool}`；`agent.cancel` 是 `{request_id: RequestId}`。后两者当前没有“作用域”字段，审批只有本次允许/拒绝。
- `session.load` 返回 `{messages, pending_approvals, active_requests}`；pending 项实际结构为 `{id, request_id, prompt}`，没有拆分后的动作名、命令或选项字段；active request 是字符串或数字 request id 数组。
- daemon EventKind 实际为 `turn_started`、`text_delta`、`tool_started`、`tool_finished`、`approval_required`、`turn_completed`。工具事件含 call id/name，结束事件另含 output；审批事件 data 为 `{approval: PendingApprovalInfo}`。
- 当前 `ApprovalBroker::request` 在审批事件接收端断开时会删除 pending 并返回错误；这与本轮“连接断开不能决定业务终态”的硬约束冲突，阶段 C 前必须将审批 truth 与单连接事件发送解耦。
- 编辑器入口 `run_stdio_adapter` 确认是纯私有 JSON-RPC 透传：解析项目自己的 `JsonRpcRequest`，把 method/params 原样交给 `DaemonClient`，并把 `ServerFrame` 原样写 stdout；没有 ACP initialize/session 方法或 server notification/request。
- Web 入口确实是 axum 0.8，现有路由仅 `/health` 与 `/v1/chat/completions`；鉴权落在 `is_authorized(HeaderMap, Option<&str>)`，HTTP/SSE 遇到审批会安全地自动拒绝。
- CLI 连接后不会自动 `session.load`；`/status` 只显示 history/active/pending 数量，审批交互仅发生在当前 `chat.send` 流收到 `approval_required` 时。
- `DaemonClient` 在单条 Unix 连接上按 request id 多路复用，但只把 Event 发给“发起该 request id 的本连接 pending channel”；新连接无法订阅既有 active request。要满足重连继续收流，需要 daemon 侧持久的请求事件广播/重放机制和一个订阅 RPC，而不能由入口伪造。
- `serve_unix_connection` 为每个连接创建独立 frame sender；客户端 EOF 后该 sender 最终关闭。当前 `chat.send` 与连接 sender 生命周期耦合，进一步确认断线恢复需要解耦请求执行与连接输出。
- WebSocket 可直接基于现有 axum 增加 `ws` feature 与 `WebSocketUpgrade`；握手后仍可复用 `DaemonClient::request_with_id`，但需并发读写以允许审批/cancel 与长 chat 同时进行。
- ACP 官方组织明确提供 Rust 实现 `agent-client-protocol`，当前稳定 wire protocol 为 v1；官方仓库包含 agent/client 示例。搜索结果显示旧单仓库曾发布 0.13.x，而官方项目后来又拆出 `rust-sdk`，因此必须以 crates.io 当前元数据和实际下载源码为准选定固定版本，不能仅凭搜索摘要猜版本。
- 项目把 crates.io 替换为 rsproxy sparse，首次裸跑 `cargo search/info` 被 Cargo 拒绝并提示指定 `--registry crates-io`；下一次查询使用该明确修正。
- crates.io 当前正式 crate 是 `agent-client-protocol 2.1.0`（Apache-2.0、官方 `agentclientprotocol/rust-sdk`），但 MSRV 为 Rust 1.88；项目当前声明 Rust 1.85，当前机器编译器为 1.98.1。采用 2.1.0 就必须明确把项目 rust-version 提升到至少 1.88，或调研是否有仍可获取且满足协议能力的 1.x 版本。
- `agent-client-protocol 2.0.0` 同样要求 Rust 1.88；搜索摘要中的旧 0.13.3 已无法通过当前 crates.io 索引获取，不能作为可靠选项。
- 已确认官方 2.1.0 是完整 SDK 而非只有 schema：导出 `Agent`/`Client` role、`Builder`、`ConnectionTo`、`Stdio`、`Lines`、typed request/notification、session helper 与权限请求能力；默认使用稳定 ACP v1，草案 v2 需显式 feature，本项目不应开启不稳定 v2。
- 官方 2.1.0 README 明确提供 `simple_agent` 示例并把 stdio/连接构建纳入 crate；因此选择官方 crate 路径优于手写 ACP 帧。项目可把 `rust-version` 从 1.85 提升到 1.88（当前工具链 1.98.1），并精确 pin `=2.1.0`。
- 已从下载后的 crate 源码核对：标准 agent 入口形态是 `Agent.builder().on_receive_request(...).connect_to(Stdio::new())`，不需要 `tokio_util::compat`；crate 自带 stdio 传输。
- SDK 的稳定 v1 session helper会在 `session/new` 后动态安装 session 消息 handler；`PromptRequest` 对应终态 `PromptResponse`，流式内容经 `SessionNotification`/`SessionUpdate::AgentMessageChunk` 主动发送。权限往返有 typed `RequestPermissionRequest/Response`，可直接实现标准方法而非私有帧。
- ACP v1 schema 的必要映射已核对：`NewSessionRequest` 要求绝对 cwd，`NewSessionResponse` 要求 SessionId；`LoadSessionRequest` 含 sessionId/cwd；`CancelNotification` 只带 sessionId；`RequestPermissionRequest` 必须关联一个 `ToolCallUpdate` 并给出 options；稳定选项种类包含 allow_once/allow_always/reject_once/reject_always。本项目 daemon 仅支持一次性 bool，故只广告 allow_once 与 reject_once，不能伪装持久授权。
- ACP 稳定 `SessionUpdate` 原生支持 agent message chunk、tool call、tool call update 和 plan；本项目当前事件没有独立 plan 事件，若未来出现无对应事件才降级文本，本轮不伪造 plan update。
- ACP request handler 会阻塞同连接后续入站分发；`session/prompt` 和 `session/load` 若在 handler 内直接等待 daemon/权限响应会死锁。正确模式是用 connection context `spawn` 后台任务并立即返回，让 responder 在任务终态响应。
- `ConnectionTo` 原生支持 typed `send_notification`、`send_request`；权限请求可在后台任务中无超时等待 client response。工具状态可准确映射为 Pending/InProgress/Completed/Failed。
- ACP prompt 的基线内容要求 Text 与 ResourceLink；本适配器会把文本直接拼接，把 ResourceLink 以名称+URI 注入文本，不广告尚未完成入站转换的 image/audio/embeddedContext 能力。
- SDK 的 `ConnectionTo::spawn` 任务生命周期与 ACP 连接绑定，适合让 prompt handler 立即返回而后台消费 daemon 流；真正跨 ACP 进程断线继续执行仍依赖阶段 C 的 daemon 事件订阅，而不是依赖该 task 存活。
- 官方 SDK 支持 `Channel::duplex()` 与 `Client.builder().connect_with(agent, ...)`，因此阶段 A 集成测试可全程进程内使用正式 ACP client/server 两侧和 mock daemon，无需依赖真实密钥或脆弱的手写 JSON。
- 危险审批集成测试可注册一个测试工具，通过真实 `ApprovalBroker` 请求授权；mock Provider 先返回该工具调用、再返回文本，由 ACP Client 的 typed `RequestPermissionRequest` handler 选择 allow_once，完整覆盖通知、权限往返和继续执行。
- ACP 适配器已按 SDK 推荐结构拆成可测试的 `build_acp_agent` 组件；正式入口连接 `Stdio`，单元集成测试可让官方 Client 直接 `connect_with` 该组件。
- daemon 重连采用新增私有 RPC `agent.subscribe {request_id}`：active turn 在 daemon 内维护取消令牌、最多 1 MiB 事件回放与 broadcast 实时流；订阅 RPC 把回放/实时事件改绑到订阅请求 ID，因此能被新 `DaemonClient` 正确路由。
- 已解决审批重放重复响应问题：订阅时按 `ApprovalBroker` 当前 pending 集合过滤已经被明确处理的旧 `approval_required` 回放；新产生的审批事件仍实时投影。
- 阶段 C 新发现：`chat.send` 在 `LoopEngine` 完成前持有 daemon history Mutex；若模型等待审批，重连端的 `session.load` 会被永久阻塞。恢复快照现从 append-only session JSONL 读取（每条消息 append 后 flush），并保留内存历史供活动 turn 使用。
- WebSocket 重连时原外部 request id 映射可能不存在，恢复事件使用 daemon active request id；客户端应以 recovery snapshot 中的 active request id 订阅/取消，测试已覆盖该语义。
- TUI 调研结论：当前 `main.rs` 默认进入普通 stdin REPL，代码中没有终端绘制层。适合新增 `src/entry/tui.rs` 作为瘦入口，通过 `DaemonClient`/现有 RPC 消费事件；不应把 LoopEngine、Provider 或安全逻辑复制到 TUI。
- 依赖选择：`ratatui 0.30.2`（MIT，MSRV 1.88，默认 crossterm backend）+ `crossterm 0.29.0`（MIT）；当前工具链为 Rust 1.98.1，满足要求。TUI 需要处理 raw mode、alternate screen、输入框、事件流、审批 y/N、Ctrl-C 和退出清理。
- TUI 优化核对：原界面默认颜色继承终端荧光绿；原滚动按原始行计数且使用 usize::MAX 哨兵，导致中文换行和 PageUp 不正确。改为显式 RGB 主题、统一按显示列宽换行、距底部行数滚动。视图独立于 RPC 层，工具结果默认折叠，Markdown 仅处理标题/强调/代码，不改会话内容。
- 终端主题兼容结论：不能把“支持 RGB 类型”当作“当前会话会正确呈现真彩色”。在 `TERM=dumb`、`NO_COLOR=1` 或终端调色板重映射环境中，固定 RGB 可能被抑制或错误降级。可靠默认值应为 `Color::Reset` 前景/背景并用 Bold/Dim 建立层级；内置 RGB 深色主题只应显式选择。
- session 现状：`SessionStore` 仅持有固定 `.my-agent/session.jsonl`，`session.new` 会把它重命名为 `.bak-时间戳`，而 `session.load` 永远只读固定活动文件。备份 session 没有切换/恢复 API，恢复后也无法保持稳定 ID。
- TUI 旧对话根因：`run_tui` 启动第一步直接调用 `recovery::load_snapshot(session.load)` 并把全部消息转成 UI 消息；CLI `chat` 入口还会运行 `recover_connection` 主动打印历史。因此“重开程序”等同于自动恢复当前 daemon session。
- 会话切换并发约束：`chat.send` 会先注册 active request，再锁 history 并在 LoopEngine 内取 turn lock。新建/恢复需要持有 active map 锁确认空闲，再持有 history 锁并切换 SessionStore 当前路径，才能阻止新 turn 在切换窗口内插入并写入错误 session。
- 设计选择：每个 session 使用稳定独立 JSONL 文件，SessionStore 保存一个 current 指针；`session.resume {session_id}` 只接受列表内安全文件名。TUI/REPL 启动显式调用 `session.new`，`/resume` 再由用户选择历史，不再自动加载旧对话。
- SQLite 评估：对于大量 session 的时间/标题/关键词检索、跨表事务和未来全文搜索，SQLite 优于遍历 JSONL；但当前单用户 daemon 下不是 `/resume` 的必要依赖。适合后续作为结构化索引与元数据层，原始大内容仍文件化，并通过迁移工具导入现有 JSONL。
- session 并发审计补充：仅用 active/history 锁仍会让 `session.load` 与 new/resume 在极窄窗口返回“旧历史 + 新 ID”的混合快照；DaemonState 增加专用 `session_switch` 互斥后，load/new/resume 的路径与 ID 观察保持一致。

## 六项通用能力补齐：基线审阅（2026-09-09）

- Provider 基线：`Provider` 仅要求 `chat`，默认 `chat_stream` 只为纯文本补发 `TextDelta`；`ProviderEvent` 当前只有 `TextDelta`；`OpenAiProvider` 在 `parse_sse`/`consume_sse_line` 内同时做 wire 解码、按 index 累积 id/name/arguments、JSON 解析并直接产出 `Response::ToolCalls`。
- 当前 tool-call 装配所有权在 provider，而非 agent。OpenAI arguments 使用 `push_str` 纯顺序拼接，没有长度优先或合法 JSON 快照替换；但没有 Started/Delta/Completed 生命周期、identity 分域、重复完成/EOF 不完整检测。
- `LoopEngine::run_turn_with_events` 收到完整 `Response::ToolCalls` 后先持久化 assistant 调用，再进入 `execute_in_waves`；只读连续段用 `buffered(8)`，写类串行。`ToolRegistry::execute` 先用自研 JSON Schema 子集校验，再调用工具；路径/命令 safety 与 approval 位于具体工具持有的 `SafetyPolicy` 中。
- 当前装配异常发生在 provider 请求返回阶段，因此不会执行本轮工具；但不存在可折回模型的 typed assembly error，错误会直接终止 turn。
- tool result 通过 `Message::tool_result` 原样使用 `ToolCall.id` 回填，provider 原始 id 同时也是内部执行 key，尚未做协议域隔离。
- `Config` 目前直接检查 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`MODEL_NAME`，没有 `API_TYPE`；daemon runtime 直接构造 `OpenAiProvider::from_env()`。
- Slash 基线：CLI `run_repl` 与 TUI `submit_input` 各有一套硬编码 `match`，帮助文本也各自维护且已发生漂移（CLI 广告 sessions/cancel，TUI 帮助未完整列出）；HTTP/OpenAI 入口和 ACP 目前不解析 slash。
- 既有命令最终都通过 daemon RPC 或公共 recovery helper 操作 session/cancel，适合把“解析 + 命令描述 + RPC 意图/统一响应”下沉，入口只保留终端特有交互渲染。
- Skill 基线：`SkillLibrary::context_for` 每次在 blocking 线程扫描目录；索引项为文件 stem/title/summary/body，标题从 `# ` 猜测、摘要从 `summary:`/`摘要：`或首段猜测；ASCII term + 中文 bigram 交集计数，substring 加 100，按 id 稳定打破平分，最多载入 3 个正文。当前索引并非常驻且读文件时正文已全部载入内存。
- daemon 请求通过 `DaemonState::handle_request` 集中分派，后台 turn、审批与 shutdown 由 Tokio task/channel/cancellation 管理；`LoopEngine::ephemeral` 是 sub-agent 的隔离历史执行入口。
- `PlanStore::persist_state` 使用同目录 `.tmp-<pid>` 写入后 `tokio::fs::rename` 原子提交，可复用于 cron store。
- `ToolRegistry` 已是 `HashMap<String, Arc<dyn Tool>>`，支持 clone、stable specs 与 subset；`register` 当前会静默覆盖同名项，MCP 注册需显式检测冲突并加稳定前缀。
- daemon runtime 的唯一装配点是 `build_daemon_state`；`DaemonState` 持有 engine/history/session/approval/active/shutdown。Unix server 的主 `select!` 监听连接、空闲、显式 shutdown、Ctrl-C，退出前等待 active turn，然后清理运行标记；cron scheduler 与 MCP manager 应由 state 持有并在 shutdown 分支显式停止。
- 现有 daemon 会在最后客户端断开且无 active turn 后约 2 秒退出，这与“常驻 cron”语义冲突；启用 cron/heartbeat 时需要让后台工作成为 daemon 存活条件，或调整生命周期为有 enabled job 时不空闲退出。
- `SubAgentTool` 用 `LoopEngine::ephemeral` + 新 `Vec<Message>` + memory-only plan 实现隔离历史，但它是模型工具且会选择受限工具子集；cron 可复用同一隔离 runner 思路，不应通过当前用户 session 的 `LoopEngine::new`。
- `ContextManager` 当前无 provider capability 判断；总是传完整 specs，多模态由 `ReadFileTool::from_env` 与 transient image message 控制。能力降级最合适放在统一 Provider trait/factory 或 LoopEngine 请求边界，不能在入口分叉。
- tracing 在 `main::init_tracing` 初始化；当前 usage 只在 OpenAI SSE 的 `consume_sse_line` 内以 debug 字段记录，无独立 metrics store。
- SessionStore 是 append-only JSONL，独立 session 由文件/指针管理；cron 不应切换该全局 current session，可直接使用 ephemeral engine 或单独的 session 文件/runner。
- 初始质量门禁：`cargo test --all-targets` 64/64 通过；严格 Clippy 与 rustfmt check 均通过。

## 项目一与项目二完成结论（2026-09-09）

- 新 Provider 契约把 wire 解码归一为 TextDelta/ToolCallStarted/ToolCallDelta/ToolCallCompleted/typed failure；OpenAI、Anthropic、Ollama 分别封装 SSE/SSE/NDJSON 差异。
- execution identity 使用 `tc1:<协议域>:<id|pos>:<值>`，wire id 用 URL-safe base64 可逆编码；OpenAI/Anthropic 出站回放只发送还原的原始 id，Ollama idless 调用仅在完整调用出现时分配流内 position。
- canonical assembler 独占 arguments：Append 永远逐字节追加，只有显式 AuthoritativeSnapshot 才覆盖；重复开始/完成/快照、匿名或迟到 delta、EOF 未完成、非法 JSON 都形成 typed failure。
- LoopEngine 在任何执行前对整批调用做存在性与 schema admission；assembly 与 admission 错误类型分离，并作为 system feedback 折回模型。本轮任一错误均不执行任何工具。
- capability 由 Provider trait 暴露；不支持工具时不注入 specs，不支持图片时统一降级为文字提示，入口/Context 不按 api_type 分叉。
- 三个本地 TCP mock 分别验证工具调用、agent 装配、tool result 协议回填和后续文本响应；全量 79/79 测试、严格 Clippy、fmt 通过。

## 项目三完成结论（2026-09-09）

- `src/slash.rs` 是命令名、alias、usage、参数规格、help 与 action 的唯一注册表；`/help` 完全由注册表生成，新增 `/ping` 只注册一次。
- daemon 新增 `slash.execute`，session 状态查询/新建/恢复等副作用仍由 DaemonState 执行；CLI/TUI 只解析统一 SlashResponse 并做各自渲染，不再保留命令 match 或平行帮助。
- ACP prompt 复用同一注册表，明确只允许 help/status/sessions/ping 四个只读命令；会话切换继续使用标准 ACP session 方法。WebSocket 可直接调用相同 daemon RPC；OpenAI HTTP prompt 不广告 slash。
- TUI 集成与正式 ACP Client 测试均断言 `/ping` 返回 pong；全量 80/80、严格 Clippy、fmt 通过。

## 项目四完成结论（2026-09-09）

- Skill 必须使用 YAML frontmatter 声明 name/version/description/keywords/scope；无效文件按项 warn 并跳过，不拖垮索引。
- SkillLibrary 以共享 RwLock 常驻元数据索引，正文仅在命中后重新读取；排序权重为精确 keyword ≫ 归一化 metadata term/bigram ≫ 正文 term/bigram，并按 version 降序、name 升序稳定打破平分。
- 本地安装器把源限制在工作区内、目标固定为 `.my-agent/skills`，用临时文件 + rename 提交；高版本覆盖、同版 skip、降级需 `--force`，remove 必须 `--confirm`。
- `/skill list/install/update/remove` 通过共享 slash + daemon 持有的 SkillLibrary 执行；坏 frontmatter、版本冲突、稳定排序和 slash 主路径均有测试。
- 新增固定依赖 `serde_yaml_ng =0.10.0`（MIT）与 `semver =1.0.28`（MIT OR Apache-2.0）；84/84、严格 Clippy、fmt 通过。

## 项目五完成结论（2026-09-09）

- `CronStore` 原子持久化 `.my-agent/cron.json`，支持 interval 与标准五段 cron、启停、删除、有限历史及重启恢复；坏文件降级为空 store，不阻断 daemon。
- `CronManager` 后台 tick 支持轻量 stagger、有限指数退避、可关闭 heartbeat；heartbeat 只做 store 自检，不调用模型。enabled job 或 heartbeat 会阻止 daemon 空闲退出，显式停止时后台任务可 join。
- `AgentCronRunner` 每次使用全新历史和独立 `.my-agent/cron-sessions` 文件；无人值守工具注册表使用独立 `UnattendedApproval`，所有需审批动作默认拒绝并写入最终运行记录。
- `/cron list/add/enable/disable/run-now/remove` 由共享 Slash 注册表与 daemon 真相执行，删除要求 `--confirm`。持久化、到点触发、失败重试、审批拒绝、五段表达式和 slash 主路径均有测试。
- 新增固定依赖 `chrono =0.4.45`、`cron =0.17.0`，两者均为 MIT OR Apache-2.0；89/89、严格 Clippy、fmt 通过。

## 项目六完成结论（2026-09-09）

- `src/mcp.rs` 是自研 stdio JSON-RPC 客户端：支持换行与 Content-Length framing，完成 `initialize → notifications/initialized → tools/list`，并把 `tools/call` 结果转换为普通工具文本。
- `.my-agent/mcp.json` 做文件级与 server 级双层隔离；非法 JSON/顶层结构只禁用 MCP，单个 server 配置/握手失败不影响其它 server。仅在 `args`、env value、cwd 展开 `${...}`，`command` 与 env key 保持原样；相对 cwd 必须落在工作区内。
- 工具以 `mcp__<server>__<tool>` 稳定前缀动态注册，schema 进入现有整批 admission；reload 原子替换动态 snapshot，`/mcp list/status/reload` 通过共享 Slash/daemon 执行。
- MCP 调用进入 `SafetyPolicy::authorize_external_action`：默认视为副作用并走审批，参数中的灾难命令硬拒，路径边界提示进入审批；不引入第三方 MCP SDK。daemon shutdown/reload 会关闭 stdin、kill/wait 子进程并清理 pending 请求。
- 测试覆盖合法/非法 server 共存、占位符边界、两种 framing、模型 tool-call→MCP→工具结果回填、默认拒绝、子进程清理与 slash 主路径。

## 最终全量验收（2026-09-09）

- `cargo fmt --all -- --check` 通过。
- `cargo build --release` 通过。
- `cargo test --all-targets`：97/97 通过。
- `cargo clippy --all-targets --all-features -- -D warnings` 通过且无 warning。
- 文档已同步 README 与 `docs/agent-system.html`；明确新增配置、slash、MCP stdio 限制和安全边界。

## TUI 交互与渲染能力补齐：现状确认（2026-09-09）

- 退出控制目前错误地复用状态文案：`run_event_loop` 第 216 行以 `state.status == "退出"` 结束；Esc（244）与 SlashResponse::Exit（407）写入相同字符串。其余“退出”仅是 UI 文案或终端清理错误上下文。
- `UiMessage` 只有 `role/content`（23-26），流式文本由 `append_assistant`（119）与最后 assistant 合并；ToolFinished（335-347）把工具名和输出拼成一个带换行的字符串，`view::message_lines`（279-295）再按行拆回。没有工具 call id、状态、耗时、消息 id 或 usage 容器。
- 输入是 `String`；`handle_key` 只支持 Esc、审批 Y/N/Enter、PageUp/Down、Ctrl+T、Ctrl+U、Ctrl+C、Backspace、字符末尾追加、Alt+Enter、且只有 `active.is_none()` 的 Enter 才会提交（304-315）。Paste 也只追加末尾（211-213）。
- 滚动为“距底部行数”：PageUp/Down 固定 ±8（256-262），view 用 `start=max_scroll-scroll`（161-166）；审批独占另一滚动值，固定 ±4（248-254）。没有 Home/End、逐行、鼠标或 follow-bottom 字段。
- `wrap_lines` 定义于 `view.rs:390`，每次 draw 都包裹输入、审批和所有消息（89、100、158-161），当前无缓存。输入光标始终按最后一行宽度计算，无法定位到编辑中的中间字符。
- 快照重建在 `from_snapshot`（73-74）和 `replace_snapshot`（111-112）均只取 active/pending 的 `.first()`，因此会静默丢失并发请求和审批。
- 主题仅有 terminal/dark；`view.rs:13-69` 的 Theme 直接携带具体色，组件引用字段尚未有 success/error/info/diff 等语义 token。硬编码 `Color` 仅集中在该映射和测试；组件还有若干直接 `Style::default` 背景组合。

### 阶段 1：类型化退出（已完成）

- `TuiState::should_quit` 是唯一循环终止条件；`request_quit` 统一供 Esc 与 SlashResponse::Exit 使用，status 仅显示“再见”。
- 新增回归测试证明任意 status 文案不会改变退出信号；TUI 定向测试与严格 Clippy 通过。

### 阶段 2：结构化 UiMessage（已完成）

- `UiMessage` 现在带稳定自增 id、创建时间、可选 token usage、内容版本；内容以 `Text` 或 `Tool(UiToolCall)` 区分。
- 工具节点保留 `tool_call_id`、名称、面向用户的中文标题、运行/成功/失败状态、输出行、展开状态与开始/结束时间；`工具执行错误:` 结果会明确标为失败，视图直接渲染卡片，不再拼接后拆分文本。
- 修正 TUI 对 daemon 事件字段的读取为 `tool_call_id`（此前误读 `call_id`）；同名工具调用现在按调用 ID 归属。
- 定向 TUI 测试与严格 Clippy 均通过。

### 阶段 3–8：交互、缓存、并发与主题（已完成）

- `InputEditor` 使用 `Vec<char>` 保存 Unicode 标量与光标，支持左右、行首尾、词级移动/删除、前向删除、多行粘贴、历史浏览；视觉坐标按 `unicode-width` 计算，CJK 不再以字节偏移定位。
- 聊天滚动保留“距底部”语义并加入 follow-bottom；PageUp/Down 翻页，Ctrl+上下逐行，Ctrl+Home/End 首尾。`MY_AGENT_TUI_MOUSE=1` 才启用鼠标滚轮捕获，默认不改变终端鼠标行为。
- 消息渲染缓存键为 `message_id + content_version + width + tool 展开态 + theme`，以 512 项为上限；会话快照替换清空缓存，内容版本变化和宽度变化自然失效。
- 普通输入在当前 turn/审批期间进入 FIFO `queued_turns`，Response 后自动启动下一条；Ctrl+K 清空尚未发送的队列。
- 活动流改为 `Vec<ActiveTurn>`、审批改为 `VecDeque`；恢复快照完整保留每个 active request 并逐一订阅，流式文本和工具卡片按请求 ID 分离，避免并发串流。
- 主题扩展为 terminal/dark/light；Theme 以 info/success/error/warm/code/diff-add/diff-remove 等语义 token 供组件使用，终端模式继续只用 Reset 色。
- 新增输入 CJK、缓存失效、浅色语义色及并发 request/审批队列回归测试。

## 多窗口独立 session（2026-09-09）

- 原 daemon 的 `history`、`SessionStore` turn lock、`LoopEngine` 和 `active` map 都是全局单例；`session.new` 在任意活动请求存在时直接返回 `-32001`，因此多个窗口无法独立运行。
- 运行时现改为 `SessionRuntime`：每个 session ID 对应固定 JSONL 路径、独立内存 history、turn lock 和 engine；共享的 provider/tools/context 只读复用。
- 活动请求键改为 `(session_id, request_id)`，快照、取消、订阅和待审批按 session 过滤；TUI、REPL、ACP 在 chat/slash/subscribe/cancel 请求中显式携带 session ID。
- 为兼容旧客户端保留无 session ID 的“legacy session”选择；新窗口不再接回其它窗口的活动请求，也不等待其它 session 的 turn lock。
- 首次验证发现新 session 创建若复用 workspace current pointer，会在另一个 session 活动时改变共享写入路径；已改为隔离创建并仅更新 daemon 内存中的 legacy session 选择，避免串写。
- 首次多 session 并发测试直接断言历史条数时遇到 append 尚未 flush 的时序；改为短轮询快照后再断言，生产代码未增加等待。
- 一次定向验证误把两个过滤器同时传给 `cargo test`，Cargo 在编译前拒绝；随后按项目既有规则改跑单个过滤器/全量 `--all-targets`，未影响生产代码。

## OpenClaude 对比研究（2026-09-09，初步）

- OpenClaude 把“前台会话”和“后台任务”明确分成两类：后台任务有独立的本地子进程、名称/状态/日志/终态记录和 `ps/logs/kill/attach` 控制面；这比单纯保留 daemon 内存 active map 更适合长任务恢复。
- OpenClaude 的 `QueryEngine`/`messageQueueManager`/`queueProcessor` 将用户输入队列、停止/中断、重试与消息提交拆开，说明当前项目可进一步统一请求生命周期状态机，而不是让 TUI 自己维护一套队列语义。
- OpenClaude 的 goal 服务把目标状态、持久化、评估器、控制器和 prompt instructions 分层，并有状态机测试；当前项目已有 PlanStore，但缺少对“目标/下一步/完成判定”的持久化控制层。
- OpenClaude 的远程 session 管理、permission bridge 和 websocket 恢复都围绕稳定 session ID 做事件重放与权限关联；这一点与当前刚完成的 `(session_id, request_id)` 隔离方向一致，可继续提取为通用 session lifecycle/status API。
- OpenClaude 的 goal 状态转换是纯函数，明确记录 `turnCount`、`lastEvaluatedMessageUuid`、最大轮次、暂停/完成原因，并把 evaluator 失败当作可持久化状态而不是异常退出；这是当前 PlanStore 最值得借鉴的可靠性模式。
- OpenClaude 的 queue manager 使用“不可变快照 + 订阅通知 + 优先级 dequeue + 过滤器”，同时保留非匹配命令，能避免 React/异步消费者因队列变化丢消息；当前 TUI 只有局部 FIFO，需要抽成 daemon 可观测的队列模型。

## OpenClaude 对比落地（2026-09-09）

- 排队请求取消：`LoopEngine` 获取 session turn lock 时同时监听 `CancellationToken`。这样同一 session 的第二个窗口/请求在前一个 turn 长时间运行时可以立即返回“请求已取消”，不会把取消请求卡在锁等待上；新增 PendingProvider 回归测试覆盖该时序。
- 计划写入串行化：`PlanStore` 的 `set/update/add` 共用 mutation mutex，保证“读当前状态→校验→原子持久化→替换内存状态”是单写者临界区，避免两个工具调用并发时后写入覆盖先写入的步骤更新；新增并发更新测试。
- session 实时状态：`SessionInfo` 增加 `status`（idle/running/waiting）、`active_requests` 和兼容性 `updated_at` 字段。daemon 从活动请求表和审批 broker 实时派生状态，session.list、CLI `/sessions`、TUI 恢复选择均展示活动状态；旧 JSONL/旧 session 清单因 serde default 保持可读取。
- 取舍：没有直接复制 OpenClaude 的后台子进程控制面或完整 goal evaluator，因为当前项目已有 cron/MCP/daemon 生命周期，先优先修复会直接影响多窗口交互可靠性的三个临界区；后台任务控制面可作为后续独立阶段。

## OpenClaude TUI 对比研究（2026-09-09）

- OpenClaude 的主 REPL 把消息区、底部 prompt、prompt footer/status line、通知/快捷键提示分层；prompt 底部槽位有最大高度约束，避免多行输入把 transcript 挤没。当前 TUI 也有五段布局，但状态提示和快捷键全部挤在一行 footer，输入区缺少明确的模式/队列/审批层级。
- OpenClaude 的消息呈现以“用户/助手/工具”三种视觉角色为核心：用户消息有清晰的输入标记，助手输出保留 Markdown/流式位置，工具调用以紧凑的 spinner/结果行呈现，详细输出可展开；当前 TUI 已有结构化 UiMessage/工具卡片，但每条消息前后额外空行较多，工具与正文的视觉层级仍不够紧凑。
- OpenClaude 的滚动模型支持 sticky bottom、离底后显示新消息分隔/跳到底部入口，并对长 transcript 使用虚拟列表；当前 TUI 已有 follow_bottom、缓存和 PgUp/Dn，但离底时只有“距底部 N 行”文字，没有明显的“新消息/回到底部”交互提示。
- OpenClaude 的 footer 会按终端宽度隐藏/折叠可选信息，把状态线、快捷键、模式提示和队列提示分别处理；当前 TUI 虽按宽度裁剪 footer，但窄终端会直接丢失状态和取消/审批提示。
- 可迁移方案：保留现有 ratatui 和协议，增加 compact transcript markers、sticky-bottom 新消息 pill、结构化 status bar、prompt 内队列/审批提示、可切换 help overlay 和更强的窄终端降级。暂不引入 OpenClaude 的 React 虚拟 DOM、远程对话框或品牌 Logo。

## OpenClaude TUI 对比落地（2026-09-09）

- transcript 现在以更紧凑的“角色标题 → 内容 → `·` 结束标记”呈现，助手正文使用 `│` 延续线，代码块使用 `┌─/└─`，工具调用保留状态色和可展开输出；这对应 OpenClaude 的 MessageResponse/工具行层级，同时不改变消息数据。
- footer 拆成状态线和快捷键线：状态线显示当前操作、活动请求、排队数量或待审批数；快捷键线按宽度降级，避免把所有信息塞进一个长字符串。
- sticky-bottom 体验增加 unread counter：用户滚离底部时新消息会累计，transcript 下方显示“X 条新消息 · 回到底部”；回到底部会清零。现有 Ctrl+End、PgUp/Dn 和 follow_bottom 语义保持不变。
- 新增 F1/Ctrl+/ 帮助浮层，集中说明发送、多行、历史、滚动、工具、队列、取消和退出操作；Esc 在帮助打开时只关闭帮助，不会误退出。
- prompt 高度改为随终端高度动态调整，最多占用一半内容区且保留状态/footer；窄终端继续使用最小布局和短提示。

## Agent 不可用诊断：第一轮源码证据（2026-09-09）

- 截图中的直接错误不是 Provider 超时，而是 `write_file` 对多个目标返回 `No such file or directory (os error 2)`；当前 `src/tools/write.rs` 只调用 `tokio::fs::write(&path, content)`，没有在写入前创建父目录。模型选择了 `src/main/resources/static/...`，该目录不存在时每个文件都会失败。
- `LoopEngine::execute_in_waves` 会把工具错误转成普通 `ToolOutput::text("工具执行错误: ...")` 并折回模型上下文；模型可以继续生成下一轮工具调用。当前最大 ReAct 轮数是 50，单次错误没有立即失败或暂停机制。
- 重复检测只在“工具名 + 参数哈希 + 结果哈希”完全一致连续三次时发送提醒，不会停止循环；截图中重复的 `plan` 调用很可能参数在变化，或模型在不同调用间反复规划，因此不会触发现有提醒。
- TUI 能看到工具事件和错误文本，但当前没有 request ID、ReAct round、Provider 首包/总耗时、最后一次模型请求状态、连续失败计数或当前 daemon log 路径；所以用户只能看到“工具执行”，无法判断卡在 Provider、工具、审批还是模型循环。
- 当前 daemon active 状态只能告诉 session 有活动请求，`session_snapshot` 能列出 active request/审批，但没有按 request 的 round/tool failure 统计；需要结合 `.my-agent/runtime/*/daemon.log`、session JSONL 和新增结构化 telemetry 才能定位长循环。

## Agent 不可用诊断：真实会话证据（2026-09-09）

- 目标工作区 `/Users/pilot/Desktop/test/test01` 的 daemon 当前是 `ready · pid=7513`，session 列表显示该 session 已 `idle`、0 个活动请求、55 条消息；因此这次不是 daemon 仍在执行，而是执行过程中 TUI 缺少及时的中间态/失败熔断反馈。
- `/var/folders/.../T/my-agent/a2ce7645ea393d41/daemon.log` 只记录了 4 条 `write_file` WARN，全部是目录不存在；默认 `RUST_LOG=warn` 没有 request/round/provider/tool 参数摘要，诊断信息不足。
- session JSONL 还原出完整链路：模型先并行调用 4 次 `write_file`，全部因 `src/main/resources/static/{css,js}` 父目录不存在失败；随后调用 `exec mkdir -p ...`，再次写入成功，最终执行验证并返回完成文本。也就是说模型最终自我修复了，但用户在失败循环阶段看不到“正在恢复/重试/还剩几轮”的明确状态。
- 该 session 的 `plan.json` 最终为 3/3 done；`target/classes/static` 也已有四个资源文件，说明产物实际生成。截图截取的是失败批次附近，不是最终终态。
- 另一个可观测性问题：`myagent sessions` 入口先执行 `config::validate_environment()`，当当前 shell 没有 API 环境变量时会直接报配置错误，无法查看已有 daemon/session 状态；使用 `API_TYPE=ollama MODEL_NAME=qwen3 myagent ... sessions` 才能读到 idle 状态。这会让排障更困难。

## Agent 不可用修复实施勘察（2026-09-09）

- `AgentEvent` 当前只有 turn/text/tool/complete 五类事件；协议层 `EventKind` 也只投影这些事件，适合新增 `RoundStarted`、`Telemetry` 或给 ToolStarted/Finished 增加 round/duration/failed 字段，但要保持旧客户端可反序列化。
- `LoopEngine::execute_one` 当前把所有工具异常包装为普通 `ToolOutput`，无法让外层区分成功/失败；应在 `ToolExecution` 保留 `failed` 与错误摘要，回合结束时根据连续失败数决定继续还是返回明确失败。
- `myagent status` 已经不依赖模型配置，但只输出 pid/socket；`RuntimePaths` 已有稳定 `daemon.log` 路径，可以直接展示 log、ready、session 根目录。
- `sessions` 的 daemon 连接路径本身不需要 provider 配置；应移除环境校验，同时允许已有 daemon 直接查询，daemon 不存在时再给出不依赖模型的启动/状态提示。

## Agent 不可用修复设计（2026-09-09）

- 采用向现有 `ToolStarted/ToolFinished` 事件追加 `round`、`duration_ms`、`success`、`error` 字段的兼容方案，不新增事件种类，避免 ACP/HTTP/旧 TUI 必须处理新枚举分支。
- `LoopEngine` 按工具执行结果统计连续失败；达到 3 次立即返回明确错误，成功工具会重置计数。写文件自动创建父目录后，正常前端脚手架不会触发该熔断。
- Provider telemetry 使用 tracing 记录每轮总耗时、首个流式增量耗时与响应类型；默认日志级别从 warn 提升到 info，`status` 输出 daemon.log 路径，新增只读 `logs` 命令便于现场排障。
- 系统提示补充“模糊前端请求的默认技术栈/目录创建/失败恢复”约束，减少模型把可恢复的文件系统错误当作长循环入口。

## Agent 不可用修复验证（2026-09-09）

- `cargo check --all-targets` 已通过。
- `cargo test --all-targets` 已通过，当前 111 项全绿；正在补充本轮新增行为的定向回归测试与文档。

## Agent 不可用修复结果（2026-09-09）

- 新增回归后 `cargo test --all-targets` 为 113/113，`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --all -- --check`、`git diff --check` 均通过。
- `cargo build --release` 与 `cargo install --path . --force` 通过；`/Users/pilot/.local/bin/myagent`、`my-agent` 均已验证为 0.1.0 最新 release。
- 真实验证：空模型环境下 `myagent --workspace /Users/pilot/Desktop/test/test01 sessions` 可直接读取本地 session 快照；`status` 和 `logs` 可在 daemon 停止时工作，不再要求 API 环境变量。
- 用户工作区的旧 daemon 已确认 idle 后停止，避免旧进程继续使用修复前二进制；下一次带有效模型配置运行 `myagent` 会自动拉起新版本。

## TUI 任务完成与工具折叠优化（2026-09-09）

- 原实现 `show_tools=false` 只隐藏工具输出，仍逐条绘制每个工具卡片；截图中的 10+ 条工具行因此占满消息区。现改为连续工具节点默认合并成一行摘要，摘要包含次数、工具名、轮次范围、状态和总耗时。
- Ctrl+T 现在控制“工具调用与输出详情”整体展开/收起；默认折叠时不再自动展开失败工具输出，避免异常时再次淹没正文。工具结果仍保留在 UI 数据和 session 中。
- Response 成功/失败会向 transcript 写入 `✓ 任务完成` / `✗ 任务未完成`，并在状态栏显示 request ID；CLI 终态额外打印 `[任务完成] request_id=...`。
- 新增 TUI 回归：默认摘要不展示工具输出，Ctrl+T 展开后可见；Response 成功后显示任务完成标记。

## Mac 快捷键提示修正（2026-09-09）

- 运行时原本同时支持 F1 与 Ctrl+/，但标题、footer 和帮助优先显示 F1；Mac 用户通常没有独立 F1 键，现已将所有用户可见提示统一为 `Ctrl+/`，F1 兼容处理仍保留。
# 2026-09-12 TUI 三项问题修复

- 用户已确认开始修复 `docs/known-issues.md` 中的 TUI-001～TUI-003。
- 仓库当前有多处未提交修改，包括 TUI、daemon、上下文和既有规划文件；本轮必须在这些改动上增量工作。
- README 描述当前 TUI 已有结构化 transcript、Ctrl+T 全局展开/折叠、活动请求状态与完成标记，因此修复重点应是现有布局与状态投影，而不是新建另一套界面。
- 附件现象已经文字化保存：短 transcript 被推到底部形成巨幅上方空白；详情模式像替换页面并丢失原 Query；运行时只有数字变化、缺少持续动画反馈。
- Ctrl+T 当前只翻转全局 `show_tools`；`draw_ui` 随后重算 transcript 行数，而 `follow_bottom=true` 会强制 `scroll=0` 并从新的 `max_scroll` 开始显示。工具输出展开新增大量行后，原 Query 因此被直接顶出视口，形成“换到详情页”的感受。
- 当前事件循环仅在键盘/鼠标事件或 daemon frame 到达时把 `dirty` 设为 true；16ms poll 超时本身不触发重绘，所以运行状态无法呈现持续的 spinner/不确定进度动画。
- 工具详情已经按消息序列内联生成，问题不是缺少内联数据结构，而是全局展开后的滚动锚点丢失；优先增加稳定的视口锚点，而不是另建详情页。
- 当前 TUI 定向基线为 16/16 通过，但现有测试只断言展开内容存在，没有验证“展开前后原 Query 仍位于视口”或滚动锚点，因此未捕获用户截图中的跳屏。
- 项目使用 `ratatui 0.30.2`；`Terminal::new` 的 viewport 默认行为仍需从 `ratatui-core` 实现核对，排除终端初始化导致首屏从当前光标位置开始的可能性。
- `ratatui-core 0.1.2` 文档明确说明 Fullscreen viewport 的 `Frame::area` 从 `(0,0)` 覆盖整个 backend；默认 `TerminalOptions` 使用默认 viewport，因此现有大空白并非有意采用 Inline viewport。
- 当前手工初始化与 Ratatui 官方初始化流程一样先进入 alternate screen，但没有显式 `terminal.clear()`；需继续确认 Terminal 首绘是否保证清屏，以及用户终端是否可能保留主屏内容。
- Ratatui 的 `Terminal` 实现拆分在 `terminal/init.rs`、`render.rs` 等子模块，前一次只读主模块未看到构造器不是实现缺失；下一步直接核对这些文件。
- `Terminal::new` 初始化的是两个“空白”缓冲区，首帧只输出相对空缓冲发生变化的单元格；如果某个终端的 alternate screen 没有清除旧内容，未被绘制的空白单元不会主动覆盖旧屏幕。初始化后显式 `terminal.clear()` 能消除这类残留空白/旧主屏内容。
- 已实现第一版：进入 TUI 后显式清屏；Ctrl+T 保存当前 transcript 顶部行并关闭 follow-bottom 后再展开；新增 ActivityPhase 与 90ms 动画 tick，状态栏显示无百分比的往返式进度条。
- 第一版修改 `cargo check --all-targets` 通过；仍需新增回归测试验证 query 可见性、首行位置、动画变化与终态停止。
- 新增 3 项 view 回归：短 transcript 标题/Query 靠近屏幕顶部；80 行工具输出展开后原 Query 和首行详情同时可见且不 follow-bottom；活动状态显示会随 tick 改变的不确定进度条，审批状态改为静态菱形。
- 完成响应测试补充断言 ActivityPhase 回到 Idle，确保终态不会继续动画。
- `cargo fmt --all` 后 TUI 定向测试 19/19 通过（原 16 项 + 新增 3 项）。
- 实现 diff 审查确认修改仅落在 TUI 状态/渲染/初始化和测试；`git diff --check` 通过。
- 严格 Clippy（all-targets、all-features、`-D warnings`）通过，无新增告警。
- 全量测试已通过：122/122（包含新增 TUI 回归），无失败或忽略项。
- 仓库没有保留把 TestBackend JSON 转为 PNG 的脚本，只有旧 `docs/tui-preview.png`；视觉验收优先使用 TestBackend 行位置/内容断言和真实 PTY 启动。
- `cargo fmt --all -- --check` 与 `cargo build --release` 均通过。
- 隔离 PTY 启动捕获到 `ESC[2J ESC[1;1H`，证明初始化显式清屏生效；标题绘制在第 2 行、引导内容从第 5 行开始，不再下沉到底部。
- PTY 中发送 Esc 后正常输出退出 alternate-screen、关闭 bracketed paste 与显示光标序列，进程状态 0 结束，终端恢复链路正常。
- PTY 测试只在隔离目录生成一个 daemon 日志；测试目录已整体移到 `/Users/pilot/.Trash/my-agent-tui-fix.sFXqlN`，可恢复，未触碰用户工作区 session。
- README 与 `docs/known-issues.md` 已同步修复后的 Ctrl+T、运行指示器和验收状态。
- `cargo install --path . --force` 已成功替换 `/Users/pilot/.cargo/bin/my-agent`；还需核对用户实际调用的 `myagent` 是否为同一二进制/链接。
- 命令解析核对：`myagent` 是指向当前仓库 `target/release/my-agent` 的符号链接；PATH 中 `my-agent` 优先解析到 `/Users/pilot/.local/bin/my-agent`，因此又用 `cargo install --root /Users/pilot/.local --path . --force` 更新该实际命令。
- 进一步审阅 `src/daemon/lifecycle.rs` 后确认它包含 daemon 指纹升级和日志落盘等语义改动，不可能由本轮 rustfmt 产生；这是本轮未编辑的独立用户工作，已完整保留。
- 使用 `cmp` 核对三份可执行文件：仓库 release、`/Users/pilot/.local/bin/my-agent`、`/Users/pilot/.cargo/bin/my-agent` 逐字节一致，且都包含本轮 Ctrl+T 修复文案。
- 最终审查将 Ctrl+T 锚点从单纯的顶部行偏移增强为“最近一条用户消息 ID + 行偏移回退”；多轮对话中会优先定位本轮原 Query。
- 增强后 TUI 定向测试仍为 19/19 通过。
- Query 锚点回归现包含“上一轮用户/Agent 消息 + 当前 Query + 80 行详情”，并断言锚点行号非零时当前 Query 仍可见；该定向测试通过。
- 将测试包装函数收窄后，最终质量门禁全部通过：122/122 全量测试、严格 Clippy、格式检查、release 构建和 `git diff --check` 均为绿色。
- 最终版已重新安装到 `/Users/pilot/.cargo/bin/my-agent` 与 `/Users/pilot/.local/bin/my-agent`；两者与仓库 `target/release/my-agent` 经 `cmp` 确认逐字节一致，`myagent`/`my-agent --version` 均正常返回 0.1.0。
