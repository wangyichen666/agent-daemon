(() => {
  "use strict";

  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];
  const state = {
    rpc: null,
    connected: false,
    workspace: "",
    defaultWorkspace: "",
    browsePath: "",
    browseParent: null,
    sessions: [],
    agentSessionId: null,
    inspectedSessionId: null,
    agentSnapshot: null,
    inspectedSnapshot: null,
    traces: [],
    inspectedMessageOffset: 0,
    inspectedMessageTotal: 0,
    inspectedMessageHasMore: false,
    inspectedTraceOffset: 0,
    inspectedTraceTotal: 0,
    inspectedTraceHasMore: false,
    traceFilter: "all",
    activeRequest: null,
    draftAssistant: null,
    activities: [],
    activeView: "agent",
    collapsedDays: new Set(),
    permissionMode: null,
    transcriptRenderPending: false,
    thinkingFinished: false,
  };

  class RpcSocket {
    constructor(token, workspace) {
      this.token = token;
      this.workspace = workspace;
      this.nextId = 1;
      this.pending = new Map();
      this.socket = null;
    }

    connect() {
      return new Promise((resolve, reject) => {
        const protocol = location.protocol === "https:" ? "wss:" : "ws:";
        const socket = new WebSocket(`${protocol}//${location.host}/ws`);
        this.socket = socket;
        const timeout = setTimeout(() => reject(new Error("连接 daemon 超时")), 8000);
        socket.addEventListener("open", () => {
          socket.send(JSON.stringify({
            type: "connect",
            token: this.token || undefined,
            workspace: this.workspace || undefined,
          }));
        });
        socket.addEventListener("message", (event) => {
          let frame;
          try { frame = JSON.parse(event.data); } catch { return; }
          if (frame.type === "connected") {
            clearTimeout(timeout);
            resolve(frame.workspace || this.workspace);
            return;
          }
          if (frame.type === "error") {
            clearTimeout(timeout);
            reject(new Error(frame.error || "连接失败"));
            return;
          }
          if (frame.type === "recovery" || frame.type === "recovery_error") return;
          const id = frame.frame === "event" ? frame.request_id : frame.id;
          const pending = this.pending.get(String(id));
          if (!pending) return;
          if (frame.frame === "event") {
            pending.onEvent?.(frame);
          } else if (frame.frame === "response") {
            this.pending.delete(String(id));
            if (frame.error) pending.reject(new Error(frame.error.message));
            else pending.resolve(frame.result);
          }
        });
        socket.addEventListener("close", () => {
          clearTimeout(timeout);
          for (const pending of this.pending.values()) pending.reject(new Error("WebSocket 已断开"));
          this.pending.clear();
          if (state.rpc === this) {
            state.connected = false;
            setConnection("error", "连接已断开");
            setAgentControls();
          }
        });
        socket.addEventListener("error", () => reject(new Error("无法连接 Web 服务")));
      });
    }

    request(method, params = {}, onEvent) {
      if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
        return { id: null, promise: Promise.reject(new Error("尚未连接 daemon")) };
      }
      const id = this.nextId++;
      const promise = new Promise((resolve, reject) => {
        this.pending.set(String(id), { resolve, reject, onEvent });
      });
      this.socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      return { id, promise };
    }

    close() { this.socket?.close(); }
  }

  async function bootstrap() {
    const requested = new URL(location.href).searchParams.get("workspace")
      || localStorage.getItem("my-agent-workspace")
      || "";
    try {
      const health = await fetchJson("/health");
      state.defaultWorkspace = health.workspace || "";
    } catch (error) {
      setConnection("error", error.message);
    }
    await connect(requested || state.defaultWorkspace);
  }

  async function connect(workspace = state.workspace) {
    state.rpc?.close();
    state.connected = false;
    setConnection("connecting", "正在连接工作区");
    setAgentControls();
    const rpc = new RpcSocket(apiToken(), workspace);
    state.rpc = rpc;
    try {
      const connectedWorkspace = await rpc.connect();
      if (state.rpc !== rpc) return;
      state.workspace = connectedWorkspace || workspace;
      state.connected = true;
      rememberWorkspace(state.workspace);
      updateWorkspaceUrl(state.workspace);
      updateWorkspaceUi();
      setConnection("online", "Agent 已连接");
      setAgentControls();
      await loadPermissionMode();
      await refreshSessions();
    } catch (error) {
      if (state.rpc !== rpc) return;
      state.connected = false;
      setConnection("error", "工作区连接失败");
      setAgentControls();
      toast(`${error.message}。请检查目录或连接设置。`);
    }
  }

  function setConnection(mode, label) {
    const node = $("#connection");
    node.className = `connection is-${mode}`;
    node.querySelector("span").textContent = label;
  }

  function setAgentControls() {
    const ready = state.connected && !state.activeRequest;
    $("#new-agent-session").disabled = !state.connected || Boolean(state.activeRequest);
    $("#prompt").disabled = !ready;
    $("#send").disabled = !ready;
    $("#inspect-current").disabled = !state.agentSessionId;
    $("#workspace-trigger").disabled = Boolean(state.activeRequest);
    $("#change-workspace").disabled = Boolean(state.activeRequest);
    $("#permission-trigger").disabled = !state.connected || Boolean(state.activeRequest);
  }

  function updateWorkspaceUi() {
    const label = directoryName(state.workspace) || "选择目录";
    $("#workspace-label").textContent = label;
    $("#workspace-label").title = state.workspace;
    $("#workspace-path").textContent = state.workspace || "—";
    $("#session-workspace").textContent = state.workspace || "尚未连接工作目录";
  }

  async function loadPermissionMode() {
    if (!state.connected) return;
    try {
      const result = await state.rpc.request("permissions.get").promise;
      state.permissionMode = result.mode;
      updatePermissionUi(result);
    } catch (error) {
      state.permissionMode = null;
      $("#permission-label").textContent = "不可用";
      toast(`读取权限模式失败：${error.message}`);
    }
  }

  function updatePermissionUi(result) {
    $("#permission-label").textContent = result.label || "未设置";
    $("#permission-label").title = result.description || "";
    $$("[data-permission-mode]").forEach((button) => {
      button.classList.toggle("active", button.dataset.permissionMode === result.mode);
    });
  }

  async function openPermissionsDialog() {
    if (!state.connected) {
      toast("连接工作区后才能切换权限模式。");
      return;
    }
    $("#permissions-dialog").showModal();
    await loadPermissionMode();
  }

  async function selectPermissionMode(mode) {
    if (!state.connected || state.activeRequest || mode === state.permissionMode) return;
    $$("[data-permission-mode]").forEach((button) => { button.disabled = true; });
    try {
      const result = await state.rpc.request("permissions.set", { mode }).promise;
      state.permissionMode = result.mode;
      updatePermissionUi(result);
      $("#permissions-dialog").close();
      toast(`已切换：${result.label}`);
    } catch (error) {
      toast(`切换权限模式失败：${error.message}`);
    } finally {
      $$("[data-permission-mode]").forEach((button) => { button.disabled = false; });
    }
  }

  async function refreshSessions() {
    if (!state.connected) return;
    const result = await state.rpc.request("session.list").promise;
    state.sessions = result.sessions || [];
    renderSessions();

    if (!state.sessions.some((item) => item.id === state.agentSessionId)) {
      state.agentSessionId = state.sessions[0]?.id || null;
    }
    if (state.agentSessionId) await loadAgentSession(state.agentSessionId);
    else resetAgentSession();

    if (!state.sessions.some((item) => item.id === state.inspectedSessionId)) {
      state.inspectedSessionId = state.sessions[0]?.id || null;
    }
    if (state.activeView === "sessions" && state.inspectedSessionId) {
      await inspectSession(state.inspectedSessionId, false);
    }
  }

  function renderSessions() {
    const query = $("#session-search").value.trim().toLocaleLowerCase();
    const filtered = state.sessions.filter((session) =>
      `${session.id} ${session.preview || ""}`.toLocaleLowerCase().includes(query)
    );
    $("#session-count").textContent = state.sessions.length;
    if (!filtered.length) {
      $("#session-list").innerHTML = `<div class="trace-empty">${state.connected ? "没有匹配的 Session。" : "连接工作区后查看 Session。"}</div>`;
      return;
    }
    const groups = new Map();
    filtered.forEach((session) => {
      const day = sessionDayKey(session);
      if (!groups.has(day)) groups.set(day, []);
      groups.get(day).push(session);
    });
    $("#session-list").innerHTML = [...groups.entries()].sort(([left], [right]) => right.localeCompare(left)).map(([day, sessions]) => `
      <section class="session-day ${state.collapsedDays.has(day) ? "is-collapsed" : ""}" data-day="${escapeAttr(day)}">
        <button class="session-day-toggle" data-day-toggle="${escapeAttr(day)}" type="button" aria-expanded="${!state.collapsedDays.has(day)}">
          <span class="day-title"><i>▾</i><strong>${escapeHtml(sessionDayLabel(day))}</strong><small>${sessions.length} 个 Session</small></span>
          <span class="day-chevron">⌄</span>
        </button>
        <div class="session-day-list">${sessions.map(renderSessionItem).join("")}</div>
      </section>`).join("");
    $$('[data-session]').forEach((button) => button.addEventListener("click", () => inspectSession(button.dataset.session)));
    $$('[data-day-toggle]').forEach((button) => button.addEventListener("click", () => toggleSessionDay(button.dataset.dayToggle)));
  }

  function renderSessionItem(session) {
    return `<button class="session-item ${session.id === state.inspectedSessionId ? "active" : ""}" data-session="${escapeAttr(session.id)}">
      <span class="session-row">
        <i class="dot ${escapeAttr(session.status || "idle")}"></i>
        <strong>${escapeHtml(shortId(session.id))}</strong>
      </span>
      <p>${escapeHtml(session.preview || "空白 Session")}</p>
      <small>${session.message_count || 0} 条消息 · ${formatRelative(session.updated_at)}</small>
    </button>`;
  }

  function toggleSessionDay(day) {
    if (state.collapsedDays.has(day)) state.collapsedDays.delete(day);
    else state.collapsedDays.add(day);
    renderSessions();
  }

  function sessionDayKey(session) {
    const timestamp = session.updated_at || session.modified_at;
    if (!timestamp) return todaySessionDayKey();
    const date = new Date(timestamp * 1000);
    if (Number.isNaN(date.getTime())) return "unknown";
    return [date.getFullYear(), String(date.getMonth() + 1).padStart(2, "0"), String(date.getDate()).padStart(2, "0")].join("-");
  }

  function todaySessionDayKey() {
    const today = new Date();
    return [today.getFullYear(), String(today.getMonth() + 1).padStart(2, "0"), String(today.getDate()).padStart(2, "0")].join("-");
  }

  function sessionDayLabel(day) {
    if (day === "unknown") return "日期未知";
    const [year, month, date] = day.split("-").map(Number);
    const value = new Date(year, month - 1, date);
    const today = new Date();
    const todayKey = todaySessionDayKey();
    if (day === todayKey) return "今天";
    const yesterday = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 1);
    const yesterdayKey = [yesterday.getFullYear(), String(yesterday.getMonth() + 1).padStart(2, "0"), String(yesterday.getDate()).padStart(2, "0")].join("-");
    if (day === yesterdayKey) return "昨天";
    return new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "long", day: "numeric", weekday: "short" }).format(value);
  }

  async function loadAgentSession(sessionId) {
    try {
      const snapshot = await state.rpc.request("session.load_page", { session_id: sessionId, offset: 0, limit: 60 }).promise;
      if (state.agentSessionId !== sessionId) return;
      state.agentSnapshot = snapshot;
      renderAgentTranscript();
      updateAgentSessionUi();
    } catch (error) {
      toast(`读取 Agent Session 失败：${error.message}`);
    }
  }

  async function inspectSession(sessionId, rerender = true) {
    state.inspectedSessionId = sessionId;
    if (rerender) renderSessions();
    $("#session-title").textContent = shortId(sessionId, 34);
    $("#session-meta").textContent = "正在读取 Session 快照与执行链路…";
    $("#continue-session").disabled = true;
    state.inspectedMessageOffset = 0;
    state.inspectedMessageTotal = 0;
    state.inspectedMessageHasMore = false;
    state.inspectedTraceOffset = 0;
    state.inspectedTraceTotal = 0;
    state.inspectedTraceHasMore = false;
    updateSessionPagers();
    try {
      const snapshotCall = state.rpc.request("session.load_page", { session_id: sessionId, offset: 0, limit: 60 }).promise;
      const traceCall = state.rpc.request("session.trace_page", { session_id: sessionId, offset: 0, limit: 60 }).promise;
      const [snapshotResult, traceResult] = await Promise.allSettled([snapshotCall, traceCall]);
      if (snapshotResult.status === "rejected") throw snapshotResult.reason;
      const snapshot = snapshotResult.value;
      const trace = traceResult.status === "fulfilled" ? traceResult.value : { records: [], total_records: 0 };
      if (traceResult.status === "rejected") toast(`链路读取失败：${traceResult.reason?.message || "未知错误"}`);
      if (state.inspectedSessionId !== sessionId) return;
      state.inspectedSnapshot = snapshot;
      state.traces = trace.records || [];
      state.inspectedMessageOffset = (snapshot.offset || 0) + state.inspectedSnapshot.messages.length;
      state.inspectedMessageTotal = snapshot.total_messages ?? state.inspectedSnapshot.messages.length;
      state.inspectedMessageHasMore = Boolean(snapshot.has_more);
      state.inspectedTraceOffset = (trace.offset || 0) + state.traces.length;
      state.inspectedTraceTotal = trace.total_records ?? state.traces.length;
      state.inspectedTraceHasMore = Boolean(trace.has_more);
      renderTranscript($("#session-transcript"), snapshot.messages || [], false);
      renderTrace();
      const status = snapshot.status || "idle";
      $("#session-meta").textContent = `${statusLabel(status)} · ${formatLoadedCount(state.inspectedMessageOffset, state.inspectedMessageTotal, "消息")} · ${formatLoadedCount(state.inspectedTraceOffset, state.inspectedTraceTotal, "链路记录")}`;
      $("#continue-session").disabled = false;
      updateSessionPagers();
    } catch (error) {
      $("#message-pager").classList.add("hidden");
      $("#trace-pager").classList.add("hidden");
      toast(`读取 Session 失败：${error.message}`);
    }
  }

  async function loadMoreMessages() {
    const sessionId = state.inspectedSessionId;
    if (!sessionId || !state.inspectedMessageHasMore) return;
    const button = $("#load-more-messages");
    button.disabled = true;
    try {
      const page = await state.rpc.request("session.load_page", {
        session_id: sessionId,
        offset: state.inspectedMessageOffset,
        limit: 60,
      }).promise;
      if (state.inspectedSessionId !== sessionId) return;
      state.inspectedSnapshot ||= { messages: [] };
      state.inspectedSnapshot.messages ||= [];
      state.inspectedSnapshot.messages.push(...(page.messages || []));
      state.inspectedMessageOffset = (page.offset || state.inspectedMessageOffset) + (page.messages || []).length;
      state.inspectedMessageTotal = page.total_messages ?? state.inspectedMessageTotal;
      state.inspectedMessageHasMore = Boolean(page.has_more);
      renderTranscript($("#session-transcript"), state.inspectedSnapshot.messages, false);
      updateSessionPagers();
      updateSessionMeta();
    } catch (error) {
      toast(`加载更多消息失败：${error.message}`);
    } finally {
      button.disabled = false;
    }
  }

  async function loadMoreTraces() {
    const sessionId = state.inspectedSessionId;
    if (!sessionId || !state.inspectedTraceHasMore) return;
    const button = $("#load-more-traces");
    button.disabled = true;
    try {
      const page = await state.rpc.request("session.trace_page", {
        session_id: sessionId,
        offset: state.inspectedTraceOffset,
        limit: 60,
      }).promise;
      if (state.inspectedSessionId !== sessionId) return;
      state.traces.push(...(page.records || []));
      state.inspectedTraceOffset = (page.offset || state.inspectedTraceOffset) + (page.records || []).length;
      state.inspectedTraceTotal = page.total_records ?? state.inspectedTraceTotal;
      state.inspectedTraceHasMore = Boolean(page.has_more);
      renderTrace();
      updateSessionPagers();
      updateSessionMeta();
    } catch (error) {
      toast(`加载更多链路失败：${error.message}`);
    } finally {
      button.disabled = false;
    }
  }

  function formatLoadedCount(loaded, total, label) {
    return total > loaded ? `已加载 ${loaded}/${total} 条${label}` : `${total} 条${label}`;
  }

  function updateSessionMeta() {
    if (!state.inspectedSnapshot) return;
    const status = state.inspectedSnapshot.status || "idle";
    $("#session-meta").textContent = `${statusLabel(status)} · ${formatLoadedCount(state.inspectedMessageOffset, state.inspectedMessageTotal, "消息")} · ${formatLoadedCount(state.inspectedTraceOffset, state.inspectedTraceTotal, "链路记录")}`;
  }

  function updateSessionPagers() {
    const messagePager = $("#message-pager");
    const tracePager = $("#trace-pager");
    messagePager.classList.toggle("hidden", !state.inspectedMessageHasMore && !state.inspectedMessageTotal);
    tracePager.classList.toggle("hidden", !state.inspectedTraceHasMore && !state.inspectedTraceTotal);
    $("#message-progress").textContent = state.inspectedMessageTotal
      ? formatLoadedCount(state.inspectedMessageOffset, state.inspectedMessageTotal, "消息")
      : "暂无消息";
    $("#trace-progress").textContent = state.inspectedTraceTotal
      ? formatLoadedCount(state.inspectedTraceOffset, state.inspectedTraceTotal, "链路")
      : "暂无链路";
    $("#load-more-messages").classList.toggle("hidden", !state.inspectedMessageHasMore);
    $("#load-more-traces").classList.toggle("hidden", !state.inspectedTraceHasMore);
  }

  function renderAgentTranscript() {
    renderTranscript($("#agent-transcript"), state.agentSnapshot?.messages || [], true);
  }

  function scheduleAgentTranscriptRender() {
    if (state.transcriptRenderPending) return;
    state.transcriptRenderPending = true;
    requestAnimationFrame(() => {
      state.transcriptRenderPending = false;
      renderAgentTranscript();
    });
  }

  function renderTranscript(node, messages, agentMode) {
    const html = messages.map((message) => renderMessage(message, agentMode)).join("");
    node.innerHTML = html || (agentMode ? agentEmptyTemplate() : `
      <div class="empty-state">
        <span class="empty-orbit">⌁</span>
        <h2>空白 Session</h2>
        <p>这个 Session 暂时没有消息。</p>
      </div>`);
    if (agentMode) bindStarterButtons();
    requestAnimationFrame(() => { node.scrollTop = node.scrollHeight; });
  }

  function agentEmptyTemplate() {
    return `<div class="empty-state agent-empty">
      <span class="empty-orbit">✦</span>
      <h2>让 Agent 真正在项目里工作</h2>
      <p>模型响应会在这里实时展示。当前任务将绑定到 <strong>${escapeHtml(state.workspace || "所选工作目录")}</strong>，你可以直接描述目标，Agent 会读取代码、执行工具并完成验证。</p>
      <div class="starter-grid">
        <button type="button" data-starter="分析这个项目的结构，并告诉我最值得优先改进的三个地方。">分析项目结构</button>
        <button type="button" data-starter="运行项目测试，定位失败原因并修复。">运行并修复测试</button>
        <button type="button" data-starter="阅读当前项目，帮我实现一个合理的小功能并完成验证。">开始开发功能</button>
      </div>
    </div>`;
  }

  function renderMessage(message) {
    const role = message.role || "system";
    const label = { user: "You", assistant: "Agent", tool: `Tool · ${message.name || "result"}`, system: "System" }[role] || role;
    const toolCalls = (message.tool_calls || []).map((call) => `
      <div class="tool-call-pill"><strong>${escapeHtml(call.name)}</strong><code>${escapeHtml(JSON.stringify(call.arguments))}</code></div>`).join("");
    const toolDetails = toolCalls ? `<details class="tool-details">
      <summary>工具调用 · ${message.tool_calls.length} 项（默认折叠）</summary>
      <div class="tool-call-list">${toolCalls}</div>
    </details>` : "";
    if (role === "tool") {
      return `<details class="message tool tool-details">
        <summary><span>${escapeHtml(label)}</span><small>输出已折叠，点击查看</small></summary>
        <div class="message-content tool-output">${escapeHtml(message.content || "无输出")}</div>
      </details>`;
    }
    const content = message.content == null ? "" : String(message.content);
    const thinking = message.thinking == null ? "" : String(message.thinking);
    const thinkingOpen = message === state.draftAssistant && !state.thinkingFinished && state.activeRequest;
    const thinkingDetails = thinking ? `<details class="thinking-details"${thinkingOpen ? " open" : ""}>
      <summary><span>思考过程</span><small>${thinkingOpen ? "生成中" : "已自动折叠"}</small></summary>
      <div class="thinking-content">${renderMarkdown(thinking)}${thinkingOpen ? `<span class="streaming-cursor" aria-label="正在生成思考"></span>` : ""}</div>
    </details>` : "";
    const renderedContent = content
      ? role === "assistant" ? renderMarkdown(content) : escapeHtml(content)
      : "";
    const streamingCursor = role === "assistant" && message === state.draftAssistant && state.activeRequest
      ? `<span class="streaming-cursor" aria-label="正在生成"></span>`
      : "";
    const contentHtml = renderedContent || streamingCursor
      ? `<div class="message-content">${renderedContent}${streamingCursor}</div>`
      : "";
    return `<article class="message ${escapeAttr(role)}">
      <div class="message-label"><span>${escapeHtml(label)}</span></div>
      ${thinkingDetails}
      ${contentHtml}
      ${toolDetails}
    </article>`;
  }

  function renderMarkdown(value) {
    const lines = String(value ?? "").replace(/\r\n?/g, "\n").split("\n");
    const blocks = [];
    let paragraph = [];
    let listType = null;
    let quoteLines = [];
    const flushParagraph = () => {
      if (!paragraph.length) return;
      blocks.push(`<p>${renderInlineMarkdown(paragraph.join("\n")).replace(/\n/g, "<br />")}</p>`);
      paragraph = [];
    };
    const flushList = () => {
      if (!listType) return;
      blocks.push(`</${listType}>`);
      listType = null;
    };
    const flushQuote = () => {
      if (!quoteLines.length) return;
      blocks.push(`<blockquote>${quoteLines.map(renderInlineMarkdown).join("<br />")}</blockquote>`);
      quoteLines = [];
    };
    for (let index = 0; index < lines.length; index += 1) {
      const line = lines[index];
      if (line.includes("|") && index + 1 < lines.length && isTableSeparator(lines[index + 1])) {
        flushParagraph(); flushList(); flushQuote();
        const headers = parseTableCells(line);
        const alignments = parseTableCells(lines[index + 1]).map(tableAlignment);
        const rows = [];
        index += 2;
        while (index < lines.length && lines[index].includes("|") && lines[index].trim()) {
          rows.push(parseTableCells(lines[index]));
          index += 1;
        }
        index -= 1;
        const headerHtml = headers.map((cell, cellIndex) => `<th class="${alignments[cellIndex] || ""}">${renderInlineMarkdown(cell)}</th>`).join("");
        const rowHtml = rows.map((row) => `<tr>${headers.map((_, cellIndex) => `<td class="${alignments[cellIndex] || ""}">${renderInlineMarkdown(row[cellIndex] || "")}</td>`).join("")}</tr>`).join("");
        blocks.push(`<table><thead><tr>${headerHtml}</tr></thead>${rowHtml ? `<tbody>${rowHtml}</tbody>` : ""}</table>`);
        continue;
      }
      const fence = line.match(/^ {0,3}(```+|~~~+)\s*([^ ]*)\s*$/);
      if (fence) {
        flushParagraph(); flushList(); flushQuote();
        const marker = fence[1][0];
        const code = [];
        index += 1;
        while (index < lines.length && !new RegExp(`^ {0,3}${marker}{3,}\\s*$`).test(lines[index])) {
          code.push(lines[index]);
          index += 1;
        }
        const language = fence[2] ? ` class="language-${escapeAttr(fence[2])}"` : "";
        blocks.push(`<pre><code${language}>${escapeHtml(code.join("\n"))}</code></pre>`);
        continue;
      }
      const heading = line.match(/^ {0,3}(#{1,6})\s+(.+?)\s*#*\s*$/);
      if (heading) {
        flushParagraph(); flushList(); flushQuote();
        const level = heading[1].length;
        blocks.push(`<h${level}>${renderInlineMarkdown(heading[2])}</h${level}>`);
        continue;
      }
      if (/^\s{0,3}((\*\s*){3,}|(-\s*){3,}|(_\s*){3,})$/.test(line)) {
        flushParagraph(); flushList(); flushQuote();
        blocks.push("<hr />");
        continue;
      }
      const unordered = line.match(/^ {0,3}[-*+]\s+(.+)$/);
      const ordered = line.match(/^ {0,3}\d+[.)]\s+(.+)$/);
      if (unordered || ordered) {
        flushParagraph(); flushQuote();
        const nextType = unordered ? "ul" : "ol";
        if (listType && listType !== nextType) flushList();
        if (!listType) {
          listType = nextType;
          blocks.push(`<${listType}>`);
        }
        blocks.push(`<li>${renderInlineMarkdown((unordered || ordered)[1])}</li>`);
        continue;
      }
      if (/^\s*$/.test(line)) {
        flushParagraph();
        flushList();
        flushQuote();
        continue;
      }
      const quote = line.match(/^ {0,3}>\s?(.*)$/);
      if (quote) {
        flushParagraph(); flushList();
        quoteLines.push(quote[1]);
        continue;
      }
      flushQuote();
      flushList();
      paragraph.push(line);
    }
    flushParagraph();
    flushList();
    flushQuote();
    return blocks.join("");
  }

  function parseTableCells(line) {
    const value = String(line ?? "").trim().replace(/^\|/, "").replace(/\|$/, "");
    return value.split("|").map((cell) => cell.trim());
  }

  function isTableSeparator(line) {
    const cells = parseTableCells(line);
    return cells.length > 0 && cells.every((cell) => /^:?-{3,}:?$/.test(cell));
  }

  function tableAlignment(cell) {
    const value = String(cell ?? "");
    if (value.startsWith(":") && value.endsWith(":")) return "align-center";
    if (value.endsWith(":")) return "align-right";
    if (value.startsWith(":")) return "align-left";
    return "";
  }

  function renderInlineMarkdown(value) {
    const source = String(value ?? "");
    const tokenPattern = /(`[^`]+`|\[[^\]]+\]\([^\s)]+(?:\s+["'][^"']*["'])?\)|\*\*[^*]+\*\*|__[^_]+__|~~[^~]+~~|\*[^*]+\*|_[^_]+_)/g;
    let output = "";
    let cursor = 0;
    let match;
    while ((match = tokenPattern.exec(source))) {
      output += escapeHtml(source.slice(cursor, match.index));
      const token = match[0];
      if (token.startsWith("`") && token.endsWith("`")) {
        output += `<code>${escapeHtml(token.slice(1, -1))}</code>`;
      } else if (token.startsWith("[") && token.includes("](")) {
        const link = token.match(/^\[([^\]]+)\]\(([^\s)]+)(?:\s+["']([^"']*)["'])?\)$/);
        const href = link && safeMarkdownUrl(link[2]);
        if (!link || !href) output += escapeHtml(token);
        else {
          const title = link[3] ? ` title="${escapeAttr(link[3])}"` : "";
          const external = /^(?:https?:|mailto:)/i.test(href) ? ` target="_blank" rel="noreferrer"` : "";
          output += `<a href="${escapeAttr(href)}"${title}${external}>${renderInlineMarkdown(link[1])}</a>`;
        }
      } else if (token.startsWith("**") || token.startsWith("__")) {
        output += `<strong>${renderInlineMarkdown(token.slice(2, -2))}</strong>`;
      } else if (token.startsWith("~~")) {
        output += `<del>${renderInlineMarkdown(token.slice(2, -2))}</del>`;
      } else {
        output += `<em>${renderInlineMarkdown(token.slice(1, -1))}</em>`;
      }
      cursor = tokenPattern.lastIndex;
    }
    return output + escapeHtml(source.slice(cursor));
  }

  function safeMarkdownUrl(value) {
    const href = String(value ?? "").trim();
    if (!href || /^(?:javascript|data|vbscript):/i.test(href)) return null;
    if (/^(?:#|\/|\.\.?\/)/.test(href)) return href;
    try {
      const protocol = new URL(href, window.location.href).protocol;
      return ["http:", "https:", "mailto:"].includes(protocol) ? href : null;
    } catch {
      return null;
    }
  }

  async function newAgentSession() {
    if (!state.connected || state.activeRequest) return;
    try {
      const snapshot = await state.rpc.request("session.new").promise;
      state.agentSessionId = snapshot.session_id;
      state.agentSnapshot = snapshot;
      state.activities = [];
      state.thinkingFinished = false;
      renderAgentTranscript();
      renderActivities();
      updateAgentSessionUi();
      await refreshSessionListOnly();
      switchView("agent");
      $("#prompt").focus();
    } catch (error) {
      toast(`新建任务失败：${error.message}`);
    }
  }

  async function ensureAgentSession() {
    if (state.agentSessionId) return state.agentSessionId;
    const snapshot = await state.rpc.request("session.new").promise;
    state.agentSessionId = snapshot.session_id;
    state.agentSnapshot = snapshot;
    updateAgentSessionUi();
    return snapshot.session_id;
  }

  async function refreshSessionListOnly() {
    const result = await state.rpc.request("session.list").promise;
    state.sessions = result.sessions || [];
    renderSessions();
  }

  function resetAgentSession() {
    state.agentSessionId = null;
    state.agentSnapshot = null;
    state.activities = [];
    state.thinkingFinished = false;
    renderAgentTranscript();
    renderActivities();
    updateAgentSessionUi();
  }

  function updateAgentSessionUi() {
    const id = state.agentSessionId;
    $("#agent-session-id").textContent = id ? shortId(id, 30) : "发送任务时自动创建";
    $("#agent-meta").textContent = id
      ? `当前任务 ${shortId(id, 26)} · 工作区 ${directoryName(state.workspace)}`
      : `准备在 ${directoryName(state.workspace) || "所选目录"} 开始新的开发任务`;
    $("#inspect-current").disabled = !id;
    setAgentControls();
  }

  async function sendPrompt(event) {
    event.preventDefault();
    const prompt = $("#prompt").value.trim();
    if (!prompt || !state.connected || state.activeRequest) return;
    let completed = false;
    try {
      const sessionId = await ensureAgentSession();
      $("#prompt").value = "";
      state.agentSnapshot ||= { messages: [] };
      state.agentSnapshot.messages ||= [];
      state.agentSnapshot.messages.push({ role: "user", content: prompt });
      state.draftAssistant = { role: "assistant", content: "" };
      state.thinkingFinished = false;
      state.agentSnapshot.messages.push(state.draftAssistant);
      state.activities = [];
      addActivity("turn", "任务已提交", "Agent 正在理解目标并规划下一步");
      renderAgentTranscript();
      setAgentStatus("running", "工作中");
      $("#cancel-turn").classList.remove("hidden");
      const call = state.rpc.request(
        "chat.send",
        { message: prompt, session_id: sessionId },
        handleAgentEvent,
      );
      state.activeRequest = call.id;
      setAgentControls();
      await call.promise;
      completed = true;
      addActivity("turn", "任务完成", "结果已写入当前 Session");
      setAgentStatus("idle", "已完成");
      toast("Agent 任务完成");
    } catch (error) {
      state.agentSnapshot ||= { messages: [] };
      state.agentSnapshot.messages ||= [];
      if (state.draftAssistant && !state.draftAssistant.content && !state.draftAssistant.thinking) {
        const draftIndex = state.agentSnapshot.messages.indexOf(state.draftAssistant);
        if (draftIndex >= 0) state.agentSnapshot.messages.splice(draftIndex, 1);
      }
      state.agentSnapshot.messages.push({ role: "system", content: `任务失败：${error.message}` });
      addActivity("error", "任务失败", error.message);
      setAgentStatus("error", "失败");
      renderAgentTranscript();
      toast(`任务失败：${error.message}`);
    } finally {
      state.activeRequest = null;
      state.draftAssistant = null;
      $("#cancel-turn").classList.add("hidden");
      setAgentControls();
      await refreshSessionListOnly().catch(() => {});
      if (completed && state.agentSessionId) await loadAgentSession(state.agentSessionId);
    }
  }

  function handleAgentEvent(frame) {
    const event = frame.event;
    const data = frame.data || {};
    if (event === "text_delta") {
      state.draftAssistant ||= { role: "assistant", content: "" };
      state.draftAssistant.content += data.delta || "";
      scheduleAgentTranscriptRender();
      setAgentStatus("running", "生成响应");
    } else if (event === "thinking_delta") {
      state.draftAssistant ||= { role: "assistant", content: "" };
      state.draftAssistant.thinking = (state.draftAssistant.thinking || "") + (data.delta || "");
      state.thinkingFinished = false;
      scheduleAgentTranscriptRender();
      setAgentStatus("running", "思考中");
    } else if (event === "thinking_finished") {
      state.thinkingFinished = true;
      scheduleAgentTranscriptRender();
      setAgentStatus("running", "生成响应");
    } else if (event === "tool_started") {
      addActivity("tool", `执行 ${data.name || "工具"}`, `第 ${data.round || "?"} 轮 · 正在运行`);
      setAgentStatus("running", "执行工具");
    } else if (event === "tool_finished") {
      addActivity(data.success === false ? "error" : "tool", `${data.name || "工具"}${data.success === false ? "失败" : "完成"}`, `${data.duration_ms || 0}ms`);
    } else if (event === "approval_required") {
      renderApproval(data.approval);
      addActivity("approval", "等待审批", data.approval?.prompt || "Agent 请求执行受保护操作");
      setAgentStatus("waiting", "待审批");
    }
  }

  function addActivity(kind, title, detail) {
    state.activities.unshift({ kind, title, detail });
    state.activities = state.activities.slice(0, 30);
    renderActivities();
  }

  function renderActivities() {
    const visible = state.activities.filter((item) => item.kind !== "tool");
    const tools = state.activities.filter((item) => item.kind === "tool");
    const toolDetails = tools.length ? `<details class="activity-tools">
      <summary>工具动态 · ${tools.length} 项（默认折叠）</summary>
      <div class="activity-tools-list">${tools.map(renderActivityItem).join("")}</div>
    </details>` : "";
    $("#activity-list").innerHTML = visible.length || toolDetails
      ? `${visible.map(renderActivityItem).join("")}${toolDetails}`
      : `<div class="activity-empty">发出任务后，这里会显示模型响应、审批状态和执行结果。</div>`;
  }

  function renderActivityItem(item) {
    return `<div class="activity-item ${escapeAttr(item.kind)}"><strong>${escapeHtml(item.title)}</strong><span>${escapeHtml(item.detail)}</span></div>`;
  }

  function setAgentStatus(mode, label) {
    const node = $("#agent-status");
    node.className = `status-pill ${mode}`;
    node.textContent = label;
  }

  function renderApproval(approval) {
    if (!approval) return;
    const card = document.createElement("div");
    card.className = "approval-card";
    card.innerHTML = `<strong>需要审批</strong><p>${escapeHtml(approval.prompt || "Agent 请求执行受保护操作")}</p>
      <div class="approval-actions"><button class="approve" type="button">允许</button><button class="deny" type="button">拒绝</button></div>`;
    card.querySelector(".approve").addEventListener("click", () => respondApproval(approval.id, true, card));
    card.querySelector(".deny").addEventListener("click", () => respondApproval(approval.id, false, card));
    $("#agent-transcript").appendChild(card);
    $("#agent-transcript").scrollTop = $("#agent-transcript").scrollHeight;
  }

  async function respondApproval(id, approved, card) {
    try {
      await state.rpc.request("approval.respond", { approval_id: id, approved }).promise;
      card.remove();
      addActivity("approval", approved ? "已允许操作" : "已拒绝操作", "Agent 将继续处理当前任务");
      setAgentStatus("running", "继续工作");
    } catch (error) {
      toast(`审批失败：${error.message}`);
    }
  }

  async function cancelTurn() {
    if (!state.activeRequest) return;
    try {
      await state.rpc.request("agent.cancel", {
        request_id: state.activeRequest,
        session_id: state.agentSessionId,
      }).promise;
      addActivity("error", "已请求停止", "等待 Agent 结束当前步骤");
      toast("已发送停止请求");
    } catch (error) {
      toast(`停止失败：${error.message}`);
    }
  }

  function continueInspectedSession() {
    if (!state.inspectedSessionId || !state.inspectedSnapshot) return;
    state.agentSessionId = state.inspectedSessionId;
    state.agentSnapshot = JSON.parse(JSON.stringify(state.inspectedSnapshot));
    state.activities = [];
    state.thinkingFinished = false;
    renderAgentTranscript();
    renderActivities();
    updateAgentSessionUi();
    switchView("agent");
    $("#prompt").focus();
  }

  function inspectCurrentAgentSession() {
    if (!state.agentSessionId) return;
    state.inspectedSessionId = state.agentSessionId;
    switchView("sessions");
  }

  function switchView(view) {
    state.activeView = view;
    $$("[data-view]").forEach((button) => button.classList.toggle("active", button.dataset.view === view));
    $$(".view").forEach((panel) => panel.classList.toggle("is-active", panel.id === `${view}-view`));
    if (view === "sessions") {
      renderSessions();
      const next = state.inspectedSessionId || state.sessions[0]?.id;
      if (next) inspectSession(next).catch((error) => toast(error.message));
    }
  }

  function renderTrace() {
    const records = state.traces || [];
    const visible = records.filter((record) => state.traceFilter === "all" || traceGroup(record.kind) === state.traceFilter);
    $("#trace-count").textContent = state.inspectedTraceTotal || records.length;
    $("#trace-list").innerHTML = visible.length
      ? visible.map(renderTraceItem).join("")
      : `<div class="trace-empty">${records.length ? "当前筛选条件下没有记录。" : "这个 Session 尚无结构化链路。旧 Session 的消息仍会正常显示。"}</div>`;
    const modelCalls = records.filter((record) => record.kind === "model_request").length;
    const toolCalls = records.filter((record) => record.kind === "tool_started").length;
    const turns = records.filter((record) => record.kind === "turn_completed");
    const totalMs = turns.reduce((sum, record) => sum + (record.duration_ms || 0), 0);
    $("#trace-summary").innerHTML = `
      <div><strong>${modelCalls}</strong><span>模型调用</span></div>
      <div><strong>${toolCalls}</strong><span>工具调用</span></div>
      <div><strong>${formatDuration(totalMs)}</strong><span>总耗时</span></div>`;
  }

  function renderTraceItem(record) {
    const group = traceGroup(record.kind);
    const failed = record.success === false ? " failed" : "";
    const info = traceInfo(record);
    return `<article class="trace-item ${group}${failed}">
      <div class="trace-title"><strong>${escapeHtml(info.title)}</strong><time>${formatTime(record.timestamp_ms)}</time></div>
      <div class="trace-meta">${escapeHtml(info.meta)}</div>
      ${info.payload == null ? "" : `<details><summary>${escapeHtml(info.detailLabel)}</summary><pre>${escapeHtml(stringify(info.payload))}</pre></details>`}
    </article>`;
  }

  function traceInfo(record) {
    switch (record.kind) {
      case "turn_started": return { title: "Turn started", meta: `request ${record.request_id}`, detailLabel: "用户输入", payload: record.input };
      case "turn_completed": return { title: record.success ? "Turn completed" : "Turn failed", meta: `request ${record.request_id} · ${formatDuration(record.duration_ms)}`, detailLabel: "错误详情", payload: record.error };
      case "model_request": return { title: `LM request · R${record.round}`, meta: `${record.provider} · ${record.messages?.length || 0} messages · ${record.tools?.length || 0} tools`, detailLabel: "查看完整提示词与工具定义", payload: { messages: record.messages, tools: record.tools } };
      case "model_response": return { title: `LM response · R${record.round}`, meta: `${record.success ? "成功" : "失败"} · ${formatDuration(record.duration_ms)}${record.first_delta_ms == null ? "" : ` · 首字 ${record.first_delta_ms}ms`}`, detailLabel: "查看完整模型响应", payload: record.response || record.error };
      case "tool_started": return { title: `${record.name} · start`, meta: `R${record.round} · ${shortId(record.tool_call_id, 18)}`, detailLabel: "查看调用参数", payload: record.arguments };
      case "tool_finished": return { title: `${record.name} · ${record.success ? "done" : "failed"}`, meta: `R${record.round} · ${formatDuration(record.duration_ms)}`, detailLabel: "查看工具输出", payload: record.output || record.error };
      default: return { title: record.kind || "Event", meta: "", detailLabel: "原始记录", payload: record };
    }
  }

  function traceGroup(kind = "") {
    if (kind.startsWith("model_")) return "model";
    if (kind.startsWith("tool_")) return "tool";
    return "turn";
  }

  async function openWorkspacePicker() {
    if (state.activeRequest) {
      toast("Agent 正在工作，结束或停止当前任务后再切换目录。");
      return;
    }
    $("#workspace-dialog").showModal();
    await browseDirectory(state.workspace || state.defaultWorkspace).catch((error) => {
      $("#directory-list").innerHTML = `<div class="directory-empty">${escapeHtml(error.message)}</div>`;
    });
  }

  async function browseDirectory(path) {
    $("#directory-list").innerHTML = `<div class="directory-empty">正在读取目录…</div>`;
    const query = path ? `?path=${encodeURIComponent(path)}` : "";
    const listing = await fetchJson(`/api/directories${query}`);
    state.browsePath = listing.path;
    state.browseParent = listing.parent || null;
    $("#browse-path").value = listing.path;
    $("#browse-current").textContent = listing.path;
    $("#browse-parent").disabled = !listing.parent;
    renderWorkspaceFavorites(listing.favorites || []);
    $("#directory-list").innerHTML = listing.directories?.length
      ? listing.directories.map((directory) => `<button class="directory-item" type="button" data-directory="${escapeAttr(directory.path)}"><span>▱</span><strong>${escapeHtml(directory.name)}</strong></button>`).join("")
      : `<div class="directory-empty">这个目录没有子文件夹。</div>`;
    $$('[data-directory]').forEach((button) => button.addEventListener("click", () => browseDirectory(button.dataset.directory).catch((error) => toast(error.message))));
  }

  function renderWorkspaceFavorites(serverFavorites) {
    const recents = recentWorkspaces();
    const favorites = [...new Set([...serverFavorites, ...recents])].filter(Boolean).slice(0, 10);
    $("#workspace-favorites").innerHTML = favorites.map((path) => `<button type="button" data-favorite="${escapeAttr(path)}" title="${escapeAttr(path)}">${escapeHtml(directoryName(path) || path)}</button>`).join("");
    $$('[data-favorite]').forEach((button) => button.addEventListener("click", () => browseDirectory(button.dataset.favorite).catch((error) => toast(error.message))));
  }

  async function selectBrowsedWorkspace() {
    if (!state.browsePath || state.browsePath === state.workspace) {
      $("#workspace-dialog").close();
      return;
    }
    $("#workspace-dialog").close();
    resetWorkspaceState();
    await connect(state.browsePath);
    switchView("agent");
  }

  function resetWorkspaceState() {
    state.sessions = [];
    state.agentSessionId = null;
    state.inspectedSessionId = null;
    state.agentSnapshot = null;
    state.inspectedSnapshot = null;
    state.traces = [];
    state.permissionMode = null;
    state.thinkingFinished = false;
    state.inspectedMessageOffset = 0;
    state.inspectedMessageTotal = 0;
    state.inspectedMessageHasMore = false;
    state.inspectedTraceOffset = 0;
    state.inspectedTraceTotal = 0;
    state.inspectedTraceHasMore = false;
    state.activities = [];
    renderSessions();
    renderAgentTranscript();
    renderActivities();
    renderTrace();
    updateAgentSessionUi();
  }

  async function fetchJson(path) {
    const response = await fetch(path, { headers: authorizationHeaders() });
    const value = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(value.error?.message || `请求失败：${response.status}`);
    return value;
  }

  function authorizationHeaders() {
    const token = apiToken();
    return token ? { Authorization: `Bearer ${token}` } : {};
  }

  function apiToken() { return localStorage.getItem("my-agent-token") || ""; }
  function recentWorkspaces() {
    try { return JSON.parse(localStorage.getItem("my-agent-workspaces") || "[]"); }
    catch { return []; }
  }
  function rememberWorkspace(path) {
    if (!path) return;
    localStorage.setItem("my-agent-workspace", path);
    const recents = [path, ...recentWorkspaces().filter((item) => item !== path)].slice(0, 8);
    localStorage.setItem("my-agent-workspaces", JSON.stringify(recents));
  }
  function updateWorkspaceUrl(path) {
    const url = new URL(location.href);
    if (path) url.searchParams.set("workspace", path);
    history.replaceState(null, "", url);
  }
  function directoryName(path = "") {
    return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
  }
  function bindStarterButtons() {
    $$('[data-starter]').forEach((button) => button.addEventListener("click", () => {
      $("#prompt").value = button.dataset.starter;
      $("#prompt").focus();
    }));
  }
  function shortId(value = "", max = 22) {
    if (value.length <= max) return value;
    const keep = Math.max(5, Math.floor((max - 1) / 2));
    return `${value.slice(0, keep)}…${value.slice(-keep)}`;
  }
  function formatTime(value) {
    if (!value) return "时间未知";
    return new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit", fractionalSecondDigits: 3 }).format(new Date(value));
  }
  function formatRelative(seconds) {
    if (!seconds) return "时间未知";
    const delta = Math.max(0, Math.floor(Date.now() / 1000 - seconds));
    if (delta < 60) return "刚刚更新";
    if (delta < 3600) return `${Math.floor(delta / 60)} 分钟前`;
    if (delta < 86400) return `${Math.floor(delta / 3600)} 小时前`;
    return `${Math.floor(delta / 86400)} 天前`;
  }
  function formatDuration(ms = 0) {
    if (ms < 1000) return `${ms}ms`;
    if (ms < 60000) return `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)}s`;
    return `${(ms / 60000).toFixed(1)}m`;
  }
  function stringify(value) { return typeof value === "string" ? value : JSON.stringify(value, null, 2); }
  function escapeHtml(value = "") { return String(value).replace(/[&<>"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char]); }
  function escapeAttr(value = "") { return escapeHtml(value); }
  function statusLabel(status) { return ({ idle: "空闲", running: "运行中", waiting: "等待审批" })[status] || status; }
  let toastTimer;
  function toast(message) {
    const node = $("#toast");
    node.textContent = message;
    node.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => node.classList.remove("show"), 3400);
  }

  $("#composer").addEventListener("submit", sendPrompt);
  $("#prompt").addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") sendPrompt(event);
  });
  $("#new-agent-session").addEventListener("click", newAgentSession);
  $("#cancel-turn").addEventListener("click", cancelTurn);
  $("#inspect-current").addEventListener("click", inspectCurrentAgentSession);
  $("#continue-session").addEventListener("click", continueInspectedSession);
  $("#load-more-messages").addEventListener("click", loadMoreMessages);
  $("#load-more-traces").addEventListener("click", loadMoreTraces);
  $("#refresh").addEventListener("click", () => refreshSessions().catch((error) => toast(error.message)));
  $("#session-search").addEventListener("input", renderSessions);
  $$("[data-view]").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
  $("#workspace-trigger").addEventListener("click", openWorkspacePicker);
  $("#change-workspace").addEventListener("click", openWorkspacePicker);
  $("#permission-trigger").addEventListener("click", openPermissionsDialog);
  $$('[data-permission-mode]').forEach((button) => button.addEventListener("click", () => selectPermissionMode(button.dataset.permissionMode)));
  $("#browse-go").addEventListener("click", () => browseDirectory($("#browse-path").value.trim()).catch((error) => toast(error.message)));
  $("#browse-path").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      browseDirectory(event.currentTarget.value.trim()).catch((error) => toast(error.message));
    }
  });
  $("#browse-parent").addEventListener("click", () => state.browseParent && browseDirectory(state.browseParent).catch((error) => toast(error.message)));
  $("#select-workspace").addEventListener("click", selectBrowsedWorkspace);
  $("#settings").addEventListener("click", () => {
    $("#token").value = apiToken();
    $("#settings-dialog").showModal();
  });
  $("#reconnect").addEventListener("click", () => {
    localStorage.setItem("my-agent-token", $("#token").value.trim());
    $("#settings-dialog").close();
    connect();
  });
  $$(".trace-filter button").forEach((button) => button.addEventListener("click", () => {
    state.traceFilter = button.dataset.filter;
    $$(".trace-filter button").forEach((item) => item.classList.toggle("active", item === button));
    renderTrace();
  }));

  bindStarterButtons();
  bootstrap();
})();
