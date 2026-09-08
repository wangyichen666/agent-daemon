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
