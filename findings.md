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
