(() => {
  "use strict";

  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];
  const state = {
    rpc: null,
    sessions: [],
    selectedId: null,
    snapshot: null,
    traces: [],
    traceFilter: "all",
    activeRequest: null,
    draftAssistant: null,
  };

  class RpcSocket {
    constructor(token) {
      this.token = token;
      this.nextId = 1;
      this.pending = new Map();
      this.socket = null;
    }

    connect() {
      return new Promise((resolve, reject) => {
        const protocol = location.protocol === "https:" ? "wss:" : "ws:";
        const socket = new WebSocket(`${protocol}//${location.host}/ws`);
        this.socket = socket;
        const timeout = setTimeout(() => reject(new Error("连接 daemon 超时")), 5000);
        socket.addEventListener("open", () => {
          socket.send(JSON.stringify({ type: "connect", token: this.token || undefined }));
        });
        socket.addEventListener("message", (event) => {
          let frame;
          try { frame = JSON.parse(event.data); } catch { return; }
          if (frame.type === "connected") {
            clearTimeout(timeout);
            resolve();
            return;
          }
          if (frame.type === "error") {
            clearTimeout(timeout);
            reject(new Error(frame.error || "连接失败"));
            return;
          }
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
          if (state.rpc === this) setConnection("error", "连接已断开");
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

  async function connect() {
    state.rpc?.close();
    setConnection("connecting", "正在连接 daemon");
    const rpc = new RpcSocket(localStorage.getItem("my-agent-token") || "");
    state.rpc = rpc;
    try {
      await rpc.connect();
      setConnection("online", "daemon 已连接");
      enableControls(true);
      await refreshSessions();
    } catch (error) {
      enableControls(false);
      setConnection("error", error.message);
      toast(`${error.message}。如已设置 API Token，请在连接设置中填写。`);
    }
  }

  function setConnection(mode, label) {
    const node = $("#connection");
    node.className = `connection is-${mode}`;
    node.lastChild.textContent = label;
  }

  function enableControls(enabled) {
    $("#new-session").disabled = !enabled;
    $("#prompt").disabled = !enabled || !state.selectedId;
    $("#send").disabled = !enabled || !state.selectedId;
  }

  async function refreshSessions(preferredId = state.selectedId) {
    const { promise } = state.rpc.request("session.list");
    const result = await promise;
    state.sessions = result.sessions || [];
    renderSessions();
    const next = state.sessions.some((item) => item.id === preferredId)
      ? preferredId
      : state.sessions[0]?.id;
    if (next) await selectSession(next, false);
  }

  function renderSessions() {
    const query = $("#session-search").value.trim().toLocaleLowerCase();
    const filtered = state.sessions.filter((session) =>
      `${session.id} ${session.preview || ""}`.toLocaleLowerCase().includes(query)
    );
    $("#session-count").textContent = state.sessions.length;
    $("#session-list").innerHTML = filtered.length
      ? filtered.map((session) => `
        <button class="session-item ${session.id === state.selectedId ? "active" : ""}" data-session="${escapeAttr(session.id)}">
          <span class="session-row">
            <i class="dot ${escapeAttr(session.status || "idle")}"></i>
            <strong>${escapeHtml(shortId(session.id))}</strong>
          </span>
          <p>${escapeHtml(session.preview || "空白 Session")}</p>
          <small>${session.message_count || 0} 条消息 · ${formatRelative(session.updated_at)}</small>
        </button>`).join("")
      : `<div class="trace-empty">没有匹配的 Session。</div>`;
    $$("[data-session]").forEach((button) => button.addEventListener("click", () => selectSession(button.dataset.session)));
  }

  async function selectSession(sessionId, rerenderList = true) {
    state.selectedId = sessionId;
    state.draftAssistant = null;
    if (rerenderList) renderSessions();
    $("#conversation-title").textContent = shortId(sessionId, 34);
    $("#conversation-meta").textContent = "正在读取 Session 快照与执行链路…";
    enableControls(true);
    try {
      const snapshotCall = state.rpc.request("session.load", { session_id: sessionId }).promise;
      const traceCall = state.rpc.request("session.trace", { session_id: sessionId }).promise;
      const [snapshot, trace] = await Promise.all([snapshotCall, traceCall]);
      if (state.selectedId !== sessionId) return;
      state.snapshot = snapshot;
      state.traces = trace.records || [];
      renderTranscript();
      renderTrace();
      const status = snapshot.status || "idle";
      $("#conversation-meta").textContent = `${statusLabel(status)} · ${snapshot.messages.length} 条消息 · ${state.traces.length} 条链路记录`;
    } catch (error) {
      toast(`读取 Session 失败：${error.message}`);
    }
  }

  function renderTranscript() {
    const messages = state.snapshot?.messages || [];
    const html = messages.map(renderMessage).join("");
    $("#transcript").innerHTML = html || `
      <div class="empty-state">
        <span class="empty-orbit">✦</span>
        <h3>这是一个新的 Session</h3>
        <p>从下方发出第一条任务。Web、TUI 和 CLI 的记录都会进入同一套本地存储。</p>
      </div>`;
    scrollTranscript();
  }

  function renderMessage(message) {
    const role = message.role || "system";
    const label = { user: "You", assistant: "Agent", tool: `Tool · ${message.name || "result"}`, system: "System" }[role] || role;
    const toolCalls = (message.tool_calls || []).map((call) => `
      <div class="tool-call-pill">${escapeHtml(call.name)}(${escapeHtml(JSON.stringify(call.arguments))})</div>`).join("");
    return `<article class="message ${escapeAttr(role)}">
      <div class="message-label"><span>${escapeHtml(label)}</span></div>
      ${message.content ? `<div class="message-content">${escapeHtml(message.content)}</div>` : ""}
      ${toolCalls ? `<div class="tool-call-list">${toolCalls}</div>` : ""}
    </article>`;
  }

  async function newSession() {
    try {
      const { promise } = state.rpc.request("session.new");
      const snapshot = await promise;
      state.selectedId = snapshot.session_id;
      await refreshSessions(snapshot.session_id);
      $("#prompt").focus();
    } catch (error) { toast(`新建会话失败：${error.message}`); }
  }

  async function sendPrompt(event) {
    event.preventDefault();
    const prompt = $("#prompt").value.trim();
    if (!prompt || !state.selectedId || state.activeRequest) return;
    $("#prompt").value = "";
    state.snapshot ||= { messages: [] };
    state.snapshot.messages.push({ role: "user", content: prompt });
    state.draftAssistant = { role: "assistant", content: "" };
    state.snapshot.messages.push(state.draftAssistant);
    renderTranscript();
    $("#send").disabled = true;
    $("#cancel-turn").classList.remove("hidden");
    $("#conversation-meta").textContent = "模型正在思考…";
    const call = state.rpc.request(
      "chat.send",
      { message: prompt, session_id: state.selectedId },
      handleAgentEvent,
    );
    state.activeRequest = call.id;
    try {
      await call.promise;
      toast("任务完成");
    } catch (error) {
      state.snapshot.messages.push({ role: "system", content: `任务失败：${error.message}` });
      toast(`任务失败：${error.message}`);
    } finally {
      state.activeRequest = null;
      state.draftAssistant = null;
      $("#cancel-turn").classList.add("hidden");
      $("#send").disabled = false;
      await refreshSessions(state.selectedId);
    }
  }

  function handleAgentEvent(frame) {
    const event = frame.event;
    const data = frame.data || {};
    if (event === "text_delta") {
      state.draftAssistant ||= { role: "assistant", content: "" };
      state.draftAssistant.content += data.delta || "";
      renderTranscript();
      $("#conversation-meta").textContent = "正在接收模型响应…";
    } else if (event === "tool_started") {
      $("#conversation-meta").textContent = `第 ${data.round || "?"} 轮 · 正在执行 ${data.name || "工具"}`;
    } else if (event === "tool_finished") {
      $("#conversation-meta").textContent = `${data.name || "工具"} ${data.success === false ? "失败" : "完成"} · ${data.duration_ms || 0}ms`;
    } else if (event === "approval_required") {
      renderApproval(data.approval);
      $("#conversation-meta").textContent = "等待你的审批";
    }
  }

  function renderApproval(approval) {
    if (!approval) return;
    const card = document.createElement("div");
    card.className = "approval-card";
    card.innerHTML = `<strong>需要审批</strong><p>${escapeHtml(approval.prompt || "Agent 请求执行受保护操作")}</p>
      <div class="approval-actions"><button class="approve">允许</button><button class="deny">拒绝</button></div>`;
    card.querySelector(".approve").addEventListener("click", () => respondApproval(approval.id, true, card));
    card.querySelector(".deny").addEventListener("click", () => respondApproval(approval.id, false, card));
    $("#transcript").appendChild(card);
    scrollTranscript();
  }

  async function respondApproval(id, approved, card) {
    try {
      await state.rpc.request("approval.respond", { approval_id: id, approved }).promise;
      card.remove();
      toast(approved ? "已允许，Agent 继续执行" : "已拒绝，Agent 继续执行");
    } catch (error) { toast(`审批失败：${error.message}`); }
  }

  async function cancelTurn() {
    if (!state.activeRequest) return;
    try {
      await state.rpc.request("agent.cancel", {
        request_id: state.activeRequest,
        session_id: state.selectedId,
      }).promise;
      toast("已发送停止请求");
    } catch (error) { toast(`停止失败：${error.message}`); }
  }

  function renderTrace() {
    const records = state.traces || [];
    const visible = records.filter((record) => state.traceFilter === "all" || traceGroup(record.kind) === state.traceFilter);
    $("#trace-count").textContent = records.length;
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
  function scrollTranscript() { requestAnimationFrame(() => { const node = $("#transcript"); node.scrollTop = node.scrollHeight; }); }
  let toastTimer;
  function toast(message) { const node = $("#toast"); node.textContent = message; node.classList.add("show"); clearTimeout(toastTimer); toastTimer = setTimeout(() => node.classList.remove("show"), 3200); }

  $("#composer").addEventListener("submit", sendPrompt);
  $("#prompt").addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") sendPrompt(event);
  });
  $("#new-session").addEventListener("click", newSession);
  $("#cancel-turn").addEventListener("click", cancelTurn);
  $("#refresh").addEventListener("click", () => refreshSessions().catch((error) => toast(error.message)));
  $("#session-search").addEventListener("input", renderSessions);
  $("#settings").addEventListener("click", () => {
    $("#token").value = localStorage.getItem("my-agent-token") || "";
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

  connect();
})();
