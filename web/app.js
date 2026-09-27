(() => {
  "use strict";

  const API_ROOT = "/api/v1";
  const RECONNECT_MAX_MS = 15_000;
  const METRICS_INTERVAL_MS = 5_000;
  const METRICS_REQUEST_TIMEOUT_MS = 10_000;
  const METRICS_STALE_MS = 15_000;
  const XTERM_MOUSE_TRACKING_MODES = new Set([9, 1000, 1002, 1003, 1005, 1006, 1015, 1016]);
  const TERMINAL_KEYS = {
    escape: "\u001b",
    tab: "\t",
    left: "\u001b[D",
    up: "\u001b[A",
    down: "\u001b[B",
    right: "\u001b[C",
  };

  const elements = {};
  const state = {
    lastTerminalByWorkspace: (() => { try { return JSON.parse(localStorage.getItem("webterm.lastTerminalByWorkspace") || "{}") || {}; } catch { return {}; } })(),
    csrf: "",
    authenticated: false,
    capabilities: {},
    workspaces: [],
    terminals: new Map(),
    expanded: new Set(),
    sessions: new Map(),
    activeId: null,
    activeWorkspaceId: null,
    ensureInFlight: new Set(),
    ensureAttempted: new Set(),
    ctrlPending: false,
    dialog: null,
    dialogTrigger: null,
    refreshInterval: 0,
    metricsInterval: 0,
    metricsInFlight: false,
    metricsController: null,
    metricsGeneration: 0,
    metrics: null,
    metricsLastSuccess: 0,
    metricsFailed: false,
    activePathWorkspaceId: null,
    activePathStickToEnd: true,
    activePathScrollFrame: 0,
    activePathClientWidth: 0,
    viewportFrame: 0,
    toastTimer: 0,
  };

  document.addEventListener("DOMContentLoaded", init);

  function init() {
    const ids = [
      "login-view", "login-form", "password", "password-toggle", "login-error", "login-submit",
      "app-view", "drawer-open", "drawer-scrim", "sidebar", "active-workspace", "active-terminal", "active-backend",
      "connection-chip", "connection-label", "logout-button", "workspace-create", "workspace-list",
      "nav-empty", "refresh-button", "refresh-status", "terminal-panel", "terminal-empty",
      "empty-open-drawer", "terminal-tabs", "terminal-add-tab", "terminal-mount", "new-output-button", "terminal-announcer", "accessory-bar",
      "status-bar", "system-metrics", "cpu-metric", "memory-metric", "metrics-freshness",
      "ctrl-key", "mouse-mode-key", "copy-key", "paste-key", "action-dialog", "action-form", "dialog-eyebrow", "dialog-title",
      "dialog-copy", "dialog-fields", "dialog-error", "dialog-submit", "dialog-cancel", "dialog-close", "toast",
    ];
    for (const id of ids) elements[toCamel(id)] = document.getElementById(id);

    elements.loginForm.addEventListener("submit", login);
    elements.passwordToggle.addEventListener("click", togglePassword);
    elements.logoutButton.addEventListener("click", () => window.WebTermLog.open());
    window.WebTermExplorer.init({state,elements,request:apiFetch,renderNavigation,layout:() => requestAnimationFrame(fitActiveTerminal),toast:showToast});
    window.WebTermLinks.init({elements,request:apiFetch});
    window.WebTermLog.init({request:apiFetch,logout});
    for (const button of [elements.cpuMetric, elements.memoryMetric]) {
      button.addEventListener("click", () => { if (state.authenticated) window.WebTermMonitor.open({request:apiFetch, trigger:button}); });
    }
    elements.drawerOpen.addEventListener("click", toggleNavigation);
    elements.drawerScrim.addEventListener("click", closeMobileDrawer);
    elements.emptyOpenDrawer.addEventListener("click", openNavigation);
    elements.refreshButton.addEventListener("click", () => refreshWorkspaces(true));
    elements.workspaceCreate.addEventListener("click", (event) => openDialog("workspace-create", null, event.currentTarget));
    elements.workspaceList.addEventListener("click", handleNavigationClick);
    elements.terminalTabs.addEventListener("click", (event) => {
      const tab = event.target.closest("[data-terminal-id]");
      if (tab) selectTerminal(tab.dataset.terminalId);
    });
    elements.terminalAddTab.addEventListener("click", (event) => {
      const terminal = state.terminals.get(state.activeId);
      const workspace = terminal?.workspace
        || state.workspaces.find((item) => item.id === state.activeWorkspaceId)
        || state.workspaces[0];
      if (workspace) createDefaultTerminal(workspace.id, event.currentTarget);
    });
    elements.newOutputButton.addEventListener("click", followLatestOutput);
    elements.accessoryBar.addEventListener("click", handleAccessoryKey);
    elements.accessoryBar.addEventListener("pointerdown", preserveTerminalFocus);
    elements.activeWorkspace.addEventListener("scroll", trackActivePathScroll, { passive: true });
    elements.actionForm.addEventListener("submit", submitDialog);
    elements.dialogCancel.addEventListener("click", closeDialog);
    elements.dialogClose.addEventListener("click", closeDialog);
    elements.actionDialog.addEventListener("cancel", (event) => {
      event.preventDefault();
      closeDialog();
    });

    window.addEventListener("online", recoverForeground);
    window.addEventListener("offline", updateConnectionStatus);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) recoverForeground();
    });
    window.addEventListener("pageshow", recoverForeground);
    window.addEventListener("resize", scheduleViewportSync);
    window.visualViewport?.addEventListener("resize", scheduleViewportSync);
    window.visualViewport?.addEventListener("scroll", scheduleViewportSync);

    if (window.ResizeObserver) {
      const observer = new ResizeObserver(() => fitActiveTerminal());
      observer.observe(elements.terminalPanel);
    }

    syncVisualViewport();
    boot();
  }

  async function boot() {
    if (typeof window.Terminal !== "function" || !window.FitAddon?.FitAddon) {
      showLogin("Terminal assets could not be loaded. Refresh the page to try again.");
      return;
    }

    state.csrf = document.querySelector('meta[name="csrf-token"]')?.content || "";
    try {
      const response = await fetch(`${API_ROOT}/session`, {
        credentials: "same-origin",
        headers: { Accept: "application/json", "X-WebTerm-Control": "1" },
        cache: "no-store",
      });
      if (response.status === 401) {
        showLogin();
        return;
      }
      if (!response.ok) throw await responseError(response);
      const body = await readJson(response);
      rememberSecurityContext(response, body);
      if (body.authenticated === false) {
        showLogin();
        return;
      }
      enterApp(body);
    } catch (error) {
      showLogin(friendlyError(error, "Unable to reach webterm."));
    }
  }

  async function login(event) {
    event.preventDefault();
    elements.loginError.textContent = "";
    const value = elements.password.value;
    if (!value) {
      elements.loginError.textContent = "Enter your password.";
      elements.password.focus();
      return;
    }

    setButtonBusy(elements.loginSubmit, true, "Signing in…");
    const requestBody = JSON.stringify({ password: value });
    elements.password.value = "";

    try {
      const response = await fetch(`${API_ROOT}/login`, {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json", Accept: "application/json", "X-WebTerm-Control": "1" },
        body: requestBody,
      });
      const body = await readJson(response);
      if (!response.ok) throw responseErrorFromBody(response, body);
      rememberSecurityContext(response, body);
      enterApp(body);
    } catch (error) {
      elements.loginError.textContent = friendlyError(error, "Sign-in failed. Try again.");
      elements.password.focus();
    } finally {
      setButtonBusy(elements.loginSubmit, false);
    }
  }

  function togglePassword() {
    const showing = elements.password.type === "text";
    elements.password.type = showing ? "password" : "text";
    elements.passwordToggle.textContent = showing ? "Show" : "Hide";
    elements.passwordToggle.setAttribute("aria-label", showing ? "Show password" : "Hide password");
    elements.password.focus({ preventScroll: true });
  }

  async function logout() {
    elements.logoutButton.disabled = true;
    try {
      await apiFetch("/logout", { method: "POST" }, true);
    } catch (error) {
      if (error.status !== 401) showToast(friendlyError(error, "Could not sign out cleanly."));
    } finally {
      elements.logoutButton.disabled = false;
      clearAuthenticatedState();
      showLogin();
    }
  }

  function enterApp(sessionBody = {}) {
    state.authenticated = true;
    mergeCapabilities(sessionBody.capabilities);
    elements.loginView.hidden = true;
    elements.appView.hidden = false;
    elements.loginError.textContent = "";
    updateCapabilityControls();
    clearInterval(state.refreshInterval);
    state.refreshInterval = window.setInterval(() => refreshWorkspaces(false), 5_000);
    startMetricsPolling();
    syncVisualViewport();
    refreshWorkspaces(true);
    if (location.pathname === "/log" || location.pathname === "/webterm/log") window.WebTermLog.open();
  }

  function showLogin(message = "") {
    state.authenticated = false;
    clearInterval(state.refreshInterval);
    state.refreshInterval = 0;
    stopMetricsPolling(true);
    elements.appView.hidden = true;
    elements.loginView.hidden = false;
    elements.loginError.textContent = message;
    requestAnimationFrame(() => elements.password.focus({ preventScroll: true }));
  }

  function clearAuthenticatedState() {
    window.WebTermMonitor?.close();
    window.WebTermExplorer?.clear();
    window.WebTermLog?.close();
    state.authenticated = false;
    stopMetricsPolling(true);
    state.csrf = "";
    state.activeId = null;
    state.activeWorkspaceId = null;
    state.workspaces = [];
    state.terminals.clear();
    for (const session of state.sessions.values()) disposeSession(session);
    state.sessions.clear();
    state.ensureInFlight.clear();
    state.ensureAttempted.clear();
    state.activePathWorkspaceId = null;
    state.activePathStickToEnd = true;
    elements.terminalMount.replaceChildren();
    elements.workspaceList.replaceChildren();
    updateConnectionStatus();
  }

  function startMetricsPolling() {
    stopMetricsPolling(false);
    refreshMetrics();
    state.metricsInterval = window.setInterval(refreshMetrics, METRICS_INTERVAL_MS);
  }

  function stopMetricsPolling(clearDisplay) {
    clearInterval(state.metricsInterval);
    state.metricsInterval = 0;
    state.metricsGeneration += 1;
    state.metricsController?.abort();
    state.metricsController = null;
    state.metricsInFlight = false;
    if (clearDisplay) {
      state.metrics = null;
      state.metricsLastSuccess = 0;
      state.metricsFailed = false;
      renderMetrics();
    }
  }

  async function refreshMetrics() {
    if (!state.authenticated) return;
    renderMetrics();
    if (state.metricsInFlight) return;

    const generation = state.metricsGeneration;
    const controller = new AbortController();
    state.metricsController = controller;
    state.metricsInFlight = true;
    let timedOut = false;
    const requestTimeout = window.setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, METRICS_REQUEST_TIMEOUT_MS);
    try {
      const { body } = await apiFetch("/metrics", { method: "GET", signal: controller.signal });
      if (!state.authenticated || generation !== state.metricsGeneration) return;
      state.metrics = {
        cpuPercent: metricNumber(body?.cpu_percent),
        memoryUsedBytes: metricNumber(body?.memory_used_bytes),
        memoryTotalBytes: metricNumber(body?.memory_total_bytes),
        memoryPercent: metricNumber(body?.memory_percent),
      };
      state.metricsLastSuccess = Date.now();
      state.metricsFailed = false;
      renderMetrics();
    } catch (error) {
      if ((timedOut || error?.name !== "AbortError") && state.authenticated && generation === state.metricsGeneration) {
        state.metricsFailed = true;
        renderMetrics();
      }
    } finally {
      clearTimeout(requestTimeout);
      if (state.metricsController === controller) {
        state.metricsController = null;
        state.metricsInFlight = false;
      }
    }
  }

  function renderMetrics() {
    if (!elements.systemMetrics) return;
    const metrics = state.metrics;
    const hasCpu = metrics?.cpuPercent !== null && metrics?.cpuPercent !== undefined;
    const hasMemory = [metrics?.memoryUsedBytes, metrics?.memoryTotalBytes, metrics?.memoryPercent]
      .some((value) => value !== null && value !== undefined);
    const hasSample = state.metricsLastSuccess > 0 && (hasCpu || hasMemory);
    const stale = hasSample && (state.metricsFailed || Date.now() - state.metricsLastSuccess >= METRICS_STALE_MS);
    const displayState = !hasSample ? "unavailable" : stale ? "stale" : "available";

    elements.systemMetrics.dataset.state = displayState;
    elements.cpuMetric.textContent = hasCpu ? `CPU ${formatPercent(metrics.cpuPercent)}` : "CPU —";
    elements.cpuMetric.title = hasCpu
      ? `CPU usage ${formatPercent(metrics.cpuPercent)}${stale ? ", stale" : ""}`
      : "CPU usage unavailable";
    elements.memoryMetric.textContent = hasMemory ? formatMemoryMetric(metrics) : "RAM —";
    elements.memoryMetric.title = hasMemory
      ? `${formatMemoryTitle(metrics)}${stale ? ", stale" : ""}`
      : "Memory usage unavailable";
    elements.metricsFreshness.hidden = displayState === "available";
    elements.metricsFreshness.textContent = displayState === "stale" ? "Stale" : "Unavailable";
  }

  function metricNumber(value) {
    if (value === null || value === undefined || value === "") return null;
    const number = Number(value);
    return Number.isFinite(number) && number >= 0 ? number : null;
  }

  function formatPercent(value) {
    return `${Number(value).toLocaleString(undefined, { maximumFractionDigits: 1 })}%`;
  }

  function formatMemoryMetric(metrics) {
    const used = metrics.memoryUsedBytes;
    const total = metrics.memoryTotalBytes;
    const percent = metrics.memoryPercent;
    if (used !== null && total !== null) {
      return `RAM ${formatBytes(used)}/${formatBytes(total)}${percent !== null ? ` · ${formatPercent(percent)}` : ""}`;
    }
    if (percent !== null) return `RAM ${formatPercent(percent)}`;
    if (used !== null) return `RAM ${formatBytes(used)} used`;
    return `RAM ${formatBytes(total)} total`;
  }

  function formatMemoryTitle(metrics) {
    return formatMemoryMetric(metrics).replace("RAM ", "Memory ");
  }

  function formatBytes(value) {
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let amount = Number(value);
    let unit = 0;
    while (amount >= 1024 && unit < units.length - 1) {
      amount /= 1024;
      unit += 1;
    }
    const digits = amount >= 100 || unit === 0 ? 0 : 1;
    return `${amount.toLocaleString(undefined, { maximumFractionDigits: digits })} ${units[unit]}`;
  }

  function scheduleViewportSync() {
    if (state.viewportFrame) return;
    state.viewportFrame = requestAnimationFrame(() => {
      state.viewportFrame = 0;
      syncVisualViewport();
    });
  }

  function syncVisualViewport() {
    const viewport = window.visualViewport;
    const height = viewport?.height || window.innerHeight || document.documentElement.clientHeight;
    const offsetTop = viewport?.offsetTop || 0;
    elements.appView.style.setProperty("--app-viewport-height", `${Math.max(1, height)}px`);
    elements.appView.style.setProperty("--app-viewport-top", `${Math.max(0, offsetTop)}px`);
    if (state.activePathStickToEnd) scrollActivePathToEnd(false);
    fitActiveTerminal();
  }

  function recoverForeground() {
    scheduleViewportSync();
    if (!state.authenticated || document.hidden) return;
    refreshWorkspaces(false);
    refreshMetrics();
    reconnectAll(true);
  }

  async function refreshWorkspaces(announce) {
    if (!state.authenticated) return;
    if (announce) elements.refreshStatus.textContent = "Refreshing…";
    elements.refreshButton.disabled = true;
    try {
      const { response, body } = await apiFetch("/workspaces", { method: "GET" });
      rememberSecurityContext(response, body);
      mergeCapabilities(body?.capabilities);
      applyWorkspacePayload(body);
      if (announce) elements.refreshStatus.textContent = "Updated";
    } catch (error) {
      if (error.status !== 401) {
        elements.refreshStatus.textContent = "Refresh failed";
        if (announce) showToast(friendlyError(error, "Unable to refresh workspaces."));
      }
    } finally {
      elements.refreshButton.disabled = false;
      if (announce) window.setTimeout(() => {
        if (elements.refreshStatus.textContent === "Updated") elements.refreshStatus.textContent = "";
      }, 1800);
    }
  }

  function applyWorkspacePayload(payload) {
    const workspaceItems = Array.isArray(payload) ? payload : payload?.workspaces || payload?.items || [];
    const looseTerminals = Array.isArray(payload?.terminals) ? payload.terminals : [];
    const terminalsByWorkspace = new Map();
    for (const terminal of looseTerminals) {
      const workspaceId = String(terminal.workspace_id ?? terminal.workspaceId ?? "");
      if (!terminalsByWorkspace.has(workspaceId)) terminalsByWorkspace.set(workspaceId, []);
      terminalsByWorkspace.get(workspaceId).push(terminal);
    }

    const seenWorkspaces = new Set();
    const normalized = workspaceItems.map((rawWorkspace) => {
      const id = String(rawWorkspace.id);
      seenWorkspaces.add(id);
      const rawTerminals = rawWorkspace.terminals || terminalsByWorkspace.get(id) || [];
      const workspace = {
        id,
        name: String(rawWorkspace.name ?? `Workspace ${id}`),
        path: String(rawWorkspace.path ?? ""),
        terminals: rawTerminals.map((rawTerminal) => normalizeTerminal(rawTerminal, id)),
      };

      return workspace;
    });

    for (const id of Array.from(state.expanded)) {
      if (!seenWorkspaces.has(id)) state.expanded.delete(id);
    }

    window.WebTermExplorer?.observe(normalized);
    state.workspaces = normalized;
    if (!state.activeWorkspaceId || !seenWorkspaces.has(state.activeWorkspaceId)) {
      state.activeWorkspaceId = normalized[0]?.id || null;
    }
    state.terminals.clear();
    for (const workspace of normalized) {
      for (const terminal of workspace.terminals) {
        terminal.workspace = workspace;
        state.terminals.set(terminal.id, terminal);
        const session = state.sessions.get(terminal.id);
        if (session) {
          session.terminal = terminal;
          session.surface.setAttribute("aria-label", `Terminal ${terminal.name} in ${workspace.name}`);
          if (terminal.status !== "running") markSessionStopped(session);
          else session.stoppedOverlay.hidden = true;
        }
      }
    }

    if (state.activeId && !state.terminals.has(state.activeId)) deactivateTerminal();
    renderNavigation();
    if (!state.activeId && !window.WebTermExplorer?.isViewerActive()) {
      const workspace = normalized.find((item) => item.id === state.activeWorkspaceId);
      const firstRunning = workspace?.terminals.find((terminal) => terminal.status === "running");
      if (firstRunning) selectTerminal(firstRunning.id);
      else {
        renderTerminalTabs();
        // Polling must never create or focus a terminal on behalf of the user.
      }
    } else {
      renderTerminalTabs();
    }
    updateCapabilityControls();
    updateActiveContext();
  }

  function normalizeTerminal(raw, workspaceId) {
    const rawBackend = String(raw.backend ?? raw.runtime_backend ?? raw.runtimeBackend ?? "native-pty").toLowerCase();
    const backend = rawBackend === "legacy-tmux" || rawBackend === "tmux" || rawBackend === "legacy"
      ? "legacy-tmux"
      : "native-pty";
    return {
      id: String(raw.id),
      workspaceId: String(raw.workspace_id ?? raw.workspaceId ?? workspaceId),
      name: String(raw.name ?? `Terminal ${raw.id}`),
      status: String(raw.status ?? "running").toLowerCase(),
      backend,
    };
  }

  function terminalBackendLabel(terminal) {
    return terminal.backend === "legacy-tmux" ? "legacy tmux" : "native PTY";
  }

  function terminalAccessibleName(terminal, unread = false) {
    const backend = terminal.backend === "legacy-tmux" ? ", legacy tmux" : "";
    return `${terminal.name}, ${terminal.status}${backend}${unread ? ", new output" : ""}`;
  }

  function renderNavigation() {
    patchKeyedList(
      elements.workspaceList,
      state.workspaces,
      (workspace) => workspace.id,
      createWorkspaceNode,
      updateWorkspaceNode,
    );
    elements.navEmpty.hidden = state.workspaces.length !== 0;
  }

  function renderTerminalTabs() {
    const active = state.terminals.get(state.activeId);
    const workspace = active?.workspace
      || state.workspaces.find((item) => item.id === state.activeWorkspaceId)
      || state.workspaces[0];
    const terminals = orderedTerminals(workspace?.terminals || []);
    patchKeyedList(
      elements.terminalTabs,
      terminals,
      (terminal) => terminal.id,
      createTerminalTab,
      updateTerminalTab,
    );
    elements.terminalAddTab.hidden = !workspace || !can("terminal", "create");
    elements.terminalAddTab.setAttribute("aria-label", workspace ? `New terminal in ${workspace.name}` : "New terminal");
    window.WebTermExplorer?.updateTabs();
  }

  function createTerminalTab(terminal, key) {
    const tab = make("button", "terminal-tab");
    tab.type = "button";
    tab.dataset.key = key;
    tab.setAttribute("role", "tab");
    const dot = make("span", "terminal-status-dot");
    dot.setAttribute("aria-hidden", "true");
    const name = make("span", "terminal-name");
    const backend = make("span", "terminal-backend");
    const unread = make("span", "terminal-unread");
    unread.setAttribute("aria-label", "New output");
    tab.append(dot, name, backend, unread);
    return tab;
  }

  function updateTerminalTab(tab, terminal) {
    const selected = state.activeId === terminal.id;
    const unread = state.sessions.get(terminal.id)?.unread || false;
    tab.dataset.terminalId = terminal.id;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
    tab.setAttribute("aria-label", terminalAccessibleName(terminal, unread));
    tab.classList.toggle("has-unread", unread);
    tab.classList.toggle("is-stopped", terminal.status !== "running");
    tab.querySelector(".terminal-status-dot").className = `terminal-status-dot is-${safeClass(terminal.status)}`;
    tab.querySelector(".terminal-name").textContent = terminal.name;
    const backend = tab.querySelector(".terminal-backend");
    backend.textContent = "tmux";
    backend.hidden = terminal.backend !== "legacy-tmux";
  }

  function patchKeyedList(parent, items, keyFor, createNode, updateNode) {
    const existing = new Map(Array.from(parent.children, (node) => [node.dataset.key, node]));
    const keep = new Set();
    let cursor = parent.firstElementChild;
    for (const item of items) {
      const key = String(keyFor(item));
      let node = existing.get(key);
      if (!node) node = createNode(item, key);
      updateNode(node, item);
      keep.add(key);
      // Only move nodes that are out of order: re-attaching resets scroll positions.
      if (node !== cursor) parent.insertBefore(node, cursor);
      cursor = node.nextElementSibling;
    }
    for (const [key, node] of existing) {
      if (!keep.has(key)) node.remove();
    }
  }

  function orderedTerminals(terminals) {
    return terminals
      .map((terminal, index) => ({ terminal, index }))
      .sort((left, right) => {
        const statusOrder = Number(left.terminal.status !== "running") - Number(right.terminal.status !== "running");
        return statusOrder || left.index - right.index;
      })
      .map((item) => item.terminal);
  }

  function createWorkspaceNode(workspace, key) {
    const group = make("section", "workspace-group"); group.dataset.key=key; group.setAttribute("role","none");
    const row=make("div","workspace-row");
    const toggle=make("button","workspace-toggle"); toggle.type="button"; toggle.dataset.action="toggle-workspace"; toggle.setAttribute("role","treeitem");
    const disclosure=make("span","disclosure",""); disclosure.innerHTML='<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><path d="m3 4 3 3 3-3"/></svg>'; disclosure.setAttribute("aria-hidden","true"); toggle.append(disclosure);
    const name=make("button","workspace-name-button"); name.type="button"; name.dataset.action="workspace-path";
    const nameText=make("span","workspace-name"); const dot=make("span","workspace-new-dot");dot.hidden=true;dot.setAttribute("aria-label","New terminal"); name.append(nameText,dot);
    const count=make("span","workspace-count");
    const mode=actionButton("workspace-mode","","Show files or terminals"); mode.classList.add("workspace-mode-button");
    const actions=make("div","workspace-actions");actions.append(actionButton("workspace-rename","✎","Rename workspace"),actionButton("workspace-delete","×","Delete workspace"));
    row.append(toggle,name,count,mode,actions);
    const path=make("div","workspace-full-path");path.hidden=true;const code=make("code","");const copy=actionButton("workspace-copy-path","⧉","Copy workspace path");path.append(code,copy);
    group.append(row,path,window.WebTermExplorer.pane());return group;
  }

  function updateWorkspaceNode(node, workspace) {
    node.dataset.id=workspace.id;const expanded=state.expanded.has(workspace.id);node.classList.toggle("is-expanded",expanded);
    const name=window.WebTermExplorer.basename(workspace.path||workspace.name);
    const toggle=node.querySelector(".workspace-toggle");toggle.dataset.id=workspace.id;toggle.setAttribute("aria-expanded",String(expanded));toggle.setAttribute("aria-label",`${expanded?"Collapse":"Expand"} ${name}`);
    const nameButton=node.querySelector(".workspace-name-button");nameButton.dataset.id=workspace.id;nameButton.title=workspace.path;nameButton.setAttribute("aria-label",`Open ${name}`);
    node.classList.toggle("is-active-workspace",workspace.id===state.activeWorkspaceId);nameButton.setAttribute("aria-current",String(workspace.id===state.activeWorkspaceId));node.querySelector(".workspace-name").textContent=name;node.querySelector(".workspace-count").textContent=String(workspace.terminals.length);
    for(const action of node.querySelectorAll(".workspace-action")){action.dataset.id=workspace.id;if(action.dataset.action==="workspace-rename")action.hidden=!can("workspace","rename");if(action.dataset.action==="workspace-delete")action.hidden=!can("workspace","delete");}
    patchKeyedList(node.querySelector(".terminal-list"),orderedTerminals(workspace.terminals),terminal=>terminal.id,createTerminalNode,updateTerminalNode);
    window.WebTermExplorer.updateWorkspace(node,workspace);
  }

  function createTerminalNode(terminal, key) {
    const row = make("div", "terminal-row");
    row.dataset.key = key;
    row.setAttribute("role", "none");
    const select = make("button", "terminal-select");
    select.type = "button";
    select.dataset.action = "select-terminal";
    select.setAttribute("role", "treeitem");
    const dot = make("span", "terminal-status-dot");
    dot.setAttribute("aria-hidden", "true");
    const name = make("span", "terminal-name");
    const backend = make("span", "terminal-backend");
    const unread = make("span", "terminal-unread");
    unread.setAttribute("aria-label", "New output");
    select.append(dot, name, backend, unread);
    const actions = make("div", "terminal-actions");
    actions.append(
      actionButton("terminal-rename", "✎", "Rename terminal"),
      actionButton("terminal-delete", "×", "Delete terminal"),
    );
    row.append(select, actions);
    return row;
  }

  function updateTerminalNode(node, terminal) {
    node.dataset.id = terminal.id;
    const selected = state.activeId === terminal.id;
    const unread = state.sessions.get(terminal.id)?.unread || false;
    node.classList.toggle("is-selected", selected);
    node.classList.toggle("has-unread", unread);
    const select = node.querySelector(".terminal-select");
    select.dataset.id = terminal.id;
    select.setAttribute("aria-current", selected ? "true" : "false");
    select.setAttribute("aria-label", terminalAccessibleName(terminal, unread));
    const dot = node.querySelector(".terminal-status-dot");
    dot.className = `terminal-status-dot is-${safeClass(terminal.status)}`;
    node.querySelector(".terminal-name").textContent = terminal.name;
    const backend = node.querySelector(".terminal-backend");
    backend.textContent = "tmux";
    backend.hidden = terminal.backend !== "legacy-tmux";
    for (const action of node.querySelectorAll(".workspace-action")) {
      action.dataset.id = terminal.id;
      if (action.dataset.action === "terminal-rename") action.hidden = !can("terminal", "rename");
      if (action.dataset.action === "terminal-delete") action.hidden = !can("terminal", "delete");
    }
    const actions = node.querySelector(".terminal-actions");
    actions.hidden = !Array.from(actions.children).some((button) => !button.hidden);
  }

  function actionButton(action, label, accessibleLabel) {
    const button = make("button", "workspace-action", label);
    button.type = "button";
    button.dataset.action = action;
    button.setAttribute("aria-label", accessibleLabel);
    button.title = accessibleLabel;
    return button;
  }

  function handleNavigationClick(event) {
    const target = event.target.closest("[data-action]");
    if (!target) return;
    const action = target.dataset.action;
    const id = target.dataset.id;
    if (action === "toggle-workspace") {
      state.activeWorkspaceId = id;
      if (state.expanded.has(id)) state.expanded.delete(id);
      else state.expanded.add(id);
      renderNavigation();
      return;
    }
    if (action === "workspace-mode") { window.WebTermExplorer.toggleMode(id); return; }
    if (action === "workspace-path") { activateWorkspace(id); return; }
    if (action === "workspace-copy-path") { window.WebTermExplorer.copyPath(id); return; }
    if (action === "select-terminal") {
      selectTerminal(id);
      return;
    }
    if (action === "terminal-create") {
      createDefaultTerminal(id, target);
      return;
    }
    openDialog(action, id, target);
  }

  function activateWorkspace(id) {
    const workspace = state.workspaces.find((item) => item.id === String(id));
    if (!workspace) return;
    state.expanded.add(workspace.id);
    const lastId = state.lastTerminalByWorkspace[workspace.id];
    const last = workspace.terminals.find((terminal) => terminal.id === lastId);
    if (last) selectTerminal(last.id);
    else enterWorkspace(workspace.id);
  }

  function rememberTerminal(terminal) {
    state.lastTerminalByWorkspace[terminal.workspaceId] = terminal.id;
    try { localStorage.setItem("webterm.lastTerminalByWorkspace", JSON.stringify(state.lastTerminalByWorkspace)); } catch {}
  }

  function enterWorkspace(id, retry) {
    const workspace = state.workspaces.find((item) => item.id === String(id));
    if (!workspace) return;
    state.activeWorkspaceId = workspace.id;
    const running = workspace.terminals.find((terminal) => terminal.status === "running");
    if (running) {
      selectTerminal(running.id);
      return;
    }
    if (state.activeId) deactivateTerminal();
    else {
      renderTerminalTabs();
      updateActiveContext();
    }
    ensureWorkspaceTerminal(workspace.id, retry);
  }

  async function ensureWorkspaceTerminal(workspaceId, retry) {
    const id = String(workspaceId);
    if (state.ensureInFlight.has(id) || (!retry && state.ensureAttempted.has(id))) return;
    state.ensureInFlight.add(id);
    state.ensureAttempted.add(id);
    try {
      const { body } = await apiFetch(`/workspaces/${encodeURIComponent(id)}/ensure-terminal`, {
        method: "POST",
        json: {},
      });
      await refreshWorkspaces(false);
      const terminalId = body?.terminal?.id;
      if (terminalId !== undefined) selectTerminal(String(terminalId));
    } catch (error) {
      if (error.status !== 401) {
        showToast(`${friendlyError(error, "Unable to start term1.")} Select the workspace to retry.`);
      }
    } finally {
      state.ensureInFlight.delete(id);
    }
  }

  async function createDefaultTerminal(workspaceId, trigger) {
    if (!workspaceId || trigger?.disabled) return;
    if (trigger) {
      trigger.disabled = true;
      trigger.setAttribute("aria-busy", "true");
    }
    try {
      const { body } = await apiFetch(`/workspaces/${encodeURIComponent(workspaceId)}/terminals`, {
        method: "POST",
        json: {},
      });
      await refreshWorkspaces(false);
      const id = body?.terminal?.id;
      if (id !== undefined) selectTerminal(String(id));
      showToast(`Terminal ${body?.terminal?.name || "created"} is ready.`);
    } catch (error) {
      if (error.status !== 401) showToast(friendlyError(error, "Unable to create a terminal."));
    } finally {
      if (trigger) {
        trigger.disabled = false;
        trigger.removeAttribute("aria-busy");
      }
    }
  }

  function selectTerminal(id) {
    const terminal = state.terminals.get(String(id));
    if (!terminal) return;

    window.WebTermExplorer?.hideViewer();
    if (state.activeId !== terminal.id) window.WebTermLinks.hide();
    state.activeId = terminal.id;
    state.activeWorkspaceId = terminal.workspaceId;
    rememberTerminal(terminal);
    elements.terminalEmpty.hidden = true;
    let session = state.sessions.get(terminal.id);
    if (!session) session = createTerminalSession(terminal);
    for (const item of state.sessions.values()) item.surface.hidden = item !== session;
    session.surface.hidden = false;

    if (isAtBottom(session)) session.unread = false;
    renderNavigation();
    renderTerminalTabs();
    updateActiveContext();
    updateNewOutputButton();
    updateMouseModeButton();
    closeMobileDrawer();
    if (terminal.status !== "running") {
      markSessionStopped(session);
    } else if (state.capabilities.terminal_websocket !== false) {
      session.stoppedOverlay.hidden = true;
      connectSession(session);
    } else {
      session.connection = "offline";
      updateConnectionStatus();
    }
    requestAnimationFrame(() => {
      fitSession(session);
      focusTerminal(session);
    });
  }

  function deactivateTerminal() {
    window.WebTermLinks.hide();
    state.activeId = null;
    for (const session of state.sessions.values()) session.surface.hidden = true;
    elements.terminalEmpty.hidden = false;
    updateActiveContext();
    renderTerminalTabs();
    updateNewOutputButton();
    updateMouseModeButton();
    updateConnectionStatus();
  }

  function guardViewportScroll(term) {
    // xterm 5.5.0's viewport can emit a NaN scroll delta while the first
    // mobile layout still has a zero-height cell. Reject it before Core
    // mutates ydisp, otherwise a valid canonical snapshot appears blank.
    // This narrow adapter is covered by the pinned-vendor native E2E test.
    const core = term._core;
    if (!core || typeof core.scrollLines !== "function") return;
    const scrollLines = core.scrollLines;
    core.scrollLines = function (amount, ...args) {
      if (!Number.isFinite(amount)) return;
      return scrollLines.call(this, amount, ...args);
    };
  }

  function createTerminalSession(terminal) {
    const surface = make("div", "terminal-surface");
    surface.dataset.terminalId = terminal.id;
    surface.setAttribute("role", "application");
    surface.setAttribute("aria-label", `Terminal ${terminal.name} in ${terminal.workspace.name}`);
    elements.terminalMount.append(surface);

    const term = new window.Terminal({
      allowProposedApi: false,
      convertEol: false,
      cursorBlink: true,
      cursorStyle: "bar",
      drawBoldTextInBrightColors: true,
      fontFamily: '"SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.18,
      letterSpacing: 0,
      macOptionIsMeta: true,
      minimumContrastRatio: 4.5,
      rightClickSelectsWord: true,
      screenReaderMode: false,
      scrollback: 10_000,
      theme: {
        background: "#090c12",
        foreground: "#dce5ef",
        cursor: "#6ee7b7",
        cursorAccent: "#090c12",
        selectionBackground: "#275c4c",
        selectionInactiveBackground: "#243845",
        black: "#111827",
        red: "#fb7185",
        green: "#6ee7b7",
        yellow: "#fbbf24",
        blue: "#7dd3fc",
        magenta: "#c4b5fd",
        cyan: "#67e8f9",
        white: "#dbe4ee",
        brightBlack: "#64748b",
        brightRed: "#fda4af",
        brightGreen: "#a7f3d0",
        brightYellow: "#fde68a",
        brightBlue: "#bae6fd",
        brightMagenta: "#ddd6fe",
        brightCyan: "#a5f3fc",
        brightWhite: "#f8fafc",
      },
    });
    guardViewportScroll(term);
    const fitAddon = new window.FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    // Forward mouse reports to apps (vim, less, Claude CLI) by default, like a standard
    // terminal. Shift+drag still selects locally; ◉ turns reporting off for this tab.
    const mouseControl = { appMode: true, replaying: false, requested: new Set() };
    term.parser.registerCsiHandler(
      { prefix: "?", final: "h" },
      (params) => trackXtermMouseMode(mouseControl, true, params),
    );
    term.parser.registerCsiHandler(
      { prefix: "?", final: "l" },
      (params) => trackXtermMouseMode(mouseControl, false, params),
    );
    const host = make("div", "terminal-host");
    surface.append(host);
    term.open(host);
    const textarea = surface.querySelector(".xterm-helper-textarea");
    textarea?.setAttribute("aria-label", `Input for terminal ${terminal.name}`);
    const stoppedOverlay = make("div", "terminal-stopped-notice", "This terminal is stopped. Delete it or select a running terminal.");
    stoppedOverlay.setAttribute("role", "status");
    stoppedOverlay.hidden = true;
    surface.append(stoppedOverlay);

    const session = {
      id: terminal.id,
      terminal,
      surface,
      term,
      fitAddon,
      socket: null,
      desired: true,
      reconnectAttempt: 0,
      reconnectTimer: 0,
      outputParts: [],
      outputFrame: 0,
      renderQueue: [],
      renderBusy: false,
      decoder: new TextDecoder(),
      messageChain: Promise.resolve(),
      unread: false,
      lastDimensions: "",
      applyingServerResize: false,
      initialSnapshotRendered: false,
      connection: "idle",
      generation: 0,
      stoppedOverlay,
      touchCleanup: null,
      mouseControl,
    };
    state.sessions.set(terminal.id, session);

    term.onData((data) => sendTerminalInput(session, applyStickyControl(data)));
    term.onResize(({ cols, rows }) => {
      session.renderRevision = (session.renderRevision || 0) + 1;
      if (!session.applyingServerResize) sendTerminalResize(session, cols, rows);
    });
    term.onScroll(() => {
      if (isAtBottom(session) && state.activeId === session.id) session.unread = false;
      updateTerminalUnread(session);
    });
    term.attachCustomKeyEventHandler((event) => handleTerminalShortcut(event, session));
    term.attachCustomWheelEventHandler((event) => handleTerminalWheel(event, session));
    session.touchCleanup = installTouchSelection(session);
    session.linkCleanup = window.WebTermLinks.attach(session);
    return session;
  }

  function trackXtermMouseMode(control, enabled, params) {
    const modes = params.map(Number);
    const onlyMouseModes = modes.length > 0 && modes.every((mode) => XTERM_MOUSE_TRACKING_MODES.has(mode));
    if (!onlyMouseModes) return false;
    if (control.replaying) return false;
    for (const mode of modes) {
      if (enabled) control.requested.add(mode);
      else control.requested.delete(mode);
    }
    return !control.appMode;
  }

  function setApplicationMouseMode(session, enabled) {
    if (!session || session.mouseControl.appMode === enabled || session.mouseControl.replaying) return;
    const control = session.mouseControl;
    const modes = Array.from(control.requested).sort((left, right) => left - right);
    if (!modes.length) {
      control.appMode = enabled;
      updateMouseModeButton();
      return;
    }
    control.replaying = true;
    if (enabled) control.appMode = true;
    const suffix = enabled ? "h" : "l";
    session.term.write(modes.map((mode) => `\u001b[?${mode}${suffix}`).join(""), () => {
      control.replaying = false;
      control.appMode = enabled;
      updateMouseModeButton();
      focusTerminal(session);
    });
  }

  function updateMouseModeButton() {
    const session = state.sessions.get(state.activeId);
    const enabled = Boolean(session?.mouseControl.appMode);
    elements.mouseModeKey.setAttribute("aria-pressed", String(enabled));
    elements.mouseModeKey.title = enabled
      ? "Mouse reports go to the app · click to select text locally (or Shift+drag)"
      : "Local text selection · click to send mouse to the app";
  }

  function installTouchSelection(session) {
    const screen = session.surface.querySelector(".xterm-screen");
    if (!screen || !window.PointerEvent) return null;
    let timer = 0;
    let selecting = false;
    let start = null;
    let origin = null;

    const stopTimer = () => {
      clearTimeout(timer);
      timer = 0;
    };
    const pointerDown = (event) => {
      if (event.pointerType !== "touch" || event.isPrimary === false || session.mouseControl.appMode) return;
      stopTimer();
      selecting = false;
      start = terminalCellFromPointer(session, event);
      origin = { x: event.clientX, y: event.clientY };
      timer = window.setTimeout(() => {
        if (!start) return;
        selecting = true;
        session.term.select(start.column, start.row, 1);
        screen.setPointerCapture?.(event.pointerId);
      }, 450);
    };
    const pointerMove = (event) => {
      if (event.pointerType !== "touch" || !start || session.mouseControl.appMode) return;
      if (!selecting) {
        if (origin && Math.hypot(event.clientX - origin.x, event.clientY - origin.y) > 8) {
          stopTimer();
          start = null;
        }
        return;
      }
      event.preventDefault();
      const end = terminalCellFromPointer(session, event);
      if (!end) return;
      const startOffset = start.row * session.term.cols + start.column;
      const endOffset = end.row * session.term.cols + end.column;
      const first = Math.min(startOffset, endOffset);
      const last = Math.max(startOffset, endOffset);
      session.term.select(first % session.term.cols, Math.floor(first / session.term.cols), last - first + 1);
    };
    const pointerUp = (event) => {
      if (event.pointerType !== "touch") return;
      stopTimer();
      selecting = false;
      start = null;
      origin = null;
    };
    screen.addEventListener("pointerdown", pointerDown);
    screen.addEventListener("pointermove", pointerMove, { passive: false });
    screen.addEventListener("pointerup", pointerUp);
    screen.addEventListener("pointercancel", pointerUp);
    return () => {
      stopTimer();
      screen.removeEventListener("pointerdown", pointerDown);
      screen.removeEventListener("pointermove", pointerMove);
      screen.removeEventListener("pointerup", pointerUp);
      screen.removeEventListener("pointercancel", pointerUp);
    };
  }

  function terminalCellFromPointer(session, event) {
    const screen = session.surface.querySelector(".xterm-screen");
    if (!screen) return null;
    const rect = screen.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    const column = Math.max(0, Math.min(session.term.cols - 1, Math.floor((event.clientX - rect.left) / rect.width * session.term.cols)));
    const viewportRow = Math.max(0, Math.min(session.term.rows - 1, Math.floor((event.clientY - rect.top) / rect.height * session.term.rows)));
    return { column, row: session.term.buffer.active.viewportY + viewportRow };
  }

  function connectSession(session) {
    if (session.terminal.status !== "running") {
      markSessionStopped(session);
      return;
    }
    session.desired = true;
    if (session.socket && (session.socket.readyState === WebSocket.OPEN || session.socket.readyState === WebSocket.CONNECTING)) return;
    if (!navigator.onLine) {
      session.connection = "offline";
      updateConnectionStatus();
      return;
    }

    clearTimeout(session.reconnectTimer);
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(`${protocol}//${location.host}${API_ROOT}/terminals/${encodeURIComponent(session.id)}/ws?proxyport=0`);
    const generation = ++session.generation;
    session.messageChain = Promise.resolve();
    session.socket = socket;
    session.connection = "connecting";
    socket.binaryType = "arraybuffer";
    updateConnectionStatus();

    socket.addEventListener("open", () => {
      if (generation !== session.generation) return;
      session.reconnectAttempt = 0;
      session.connection = "online";
      session.lastDimensions = "";
      session.initialSnapshotRendered = false;
      session.decoder = new TextDecoder();
      if (session.terminal.backend === "legacy-tmux") {
        fitSession(session);
        sendTerminalResize(session, session.term.cols, session.term.rows);
      }
      updateConnectionStatus();
    });

    socket.addEventListener("message", (event) => {
      session.messageChain = session.messageChain.then(() => {
        if (generation === session.generation) return handleSocketMessage(session, event.data);
        return undefined;
      }).catch(() => {
        if (generation === session.generation) showToast("Terminal output could not be rendered.");
      });
    });

    socket.addEventListener("close", (event) => {
      if (generation !== session.generation) return;
      session.socket = null;
      if (session.connection !== "stopped") session.connection = "offline";
      updateConnectionStatus();
      if (session.connection === "stopped") return;
      if (event.code === 4401 || event.code === 1008) {
        verifySessionAfterAuthClose();
        return;
      }
      scheduleReconnect(session);
    });

    socket.addEventListener("error", () => {
      if (generation === session.generation) session.connection = "offline";
      updateConnectionStatus();
    });
  }

  async function verifySessionAfterAuthClose() {
    try {
      const response = await fetch(`${API_ROOT}/session`, { credentials: "same-origin", cache: "no-store" });
      if (response.status === 401) {
        clearAuthenticatedState();
        showLogin("Your session expired. Sign in again.");
      }
    } catch (_) {
      // A network failure is handled by normal reconnect logic.
    }
  }

  function scheduleReconnect(session) {
    if (!session.desired || !state.authenticated || session.terminal.status !== "running" || session.reconnectTimer) return;
    const delay = Math.min(RECONNECT_MAX_MS, 500 * (2 ** Math.min(session.reconnectAttempt, 5))) + Math.floor(Math.random() * 250);
    session.reconnectAttempt += 1;
    session.reconnectTimer = window.setTimeout(() => {
      session.reconnectTimer = 0;
      connectSession(session);
    }, delay);
  }

  function reconnectAll(replaceTransport = false) {
    if (!state.authenticated) return;
    for (const session of state.sessions.values()) {
      if (!session.desired || session.terminal.status !== "running") continue;
      if (replaceTransport) {
        // Mobile sleep may leave a dead connection reporting OPEN. Replace only
        // its transport; never dispose the xterm instance or stop its server-owned shell.
        session.generation += 1;
        const previous = session.socket;
        session.socket = null;
        clearTimeout(session.reconnectTimer);
        session.reconnectTimer = 0;
        try { previous?.close(1000, "foreground reconnect"); } catch (_) { /* already closed */ }
      }
      connectSession(session);
      if (session.socket?.readyState === WebSocket.OPEN) {
        session.lastDimensions = "";
        sendTerminalResize(session, session.term.cols, session.term.rows);
      }
    }
  }

  async function handleSocketMessage(session, data) {
    if (data instanceof ArrayBuffer) {
      enqueueOutput(session, session.decoder.decode(new Uint8Array(data), { stream: true }));
      return;
    }
    if (data instanceof Blob) {
      const buffer = await data.arrayBuffer();
      enqueueOutput(session, session.decoder.decode(new Uint8Array(buffer), { stream: true }));
      return;
    }

    let message;
    try {
      message = JSON.parse(data);
    } catch (_) {
      enqueueOutput(session, String(data));
      return;
    }

    const type = String(message.type ?? message.event ?? "").replace("terminal.", "");
    if (type === "output") {
      enqueueOutput(session, String(message.data ?? message.output ?? ""));
    } else if (type === "snapshot") {
      enqueueSnapshot(session, String(message.data ?? message.output ?? ""));
    } else if (type === "resize") {
      applyServerResize(session, Number(message.cols), Number(message.rows));
    } else if (type === "status") {
      const status = String(message.status || "").toLowerCase();
      if (status === "connected") session.connection = "online";
      if (status === "detached" && session.connection !== "stopped") session.connection = "offline";
      updateConnectionStatus();
    } else if (type === "closed") {
      session.desired = false;
      session.connection = "stopped";
      session.terminal.status = "stopped";
      session.stoppedOverlay.hidden = false;
      updateConnectionStatus();
      refreshWorkspaces(false);
    } else if (type === "error") {
      showToast(String(message.message || "Terminal connection error."));
    }
  }

  function enqueueOutput(session, data) {
    if (!data) return;
    session.outputParts.push(data);
    if (session.outputFrame) return;
    session.outputFrame = requestAnimationFrame(() => flushOutput(session));
  }

  function flushOutput(session) {
    session.outputFrame = 0;
    if (!session.outputParts.length) return;
    const output = session.outputParts.join("");
    session.outputParts.length = 0;
    session.renderQueue.push({ kind: "output", data: output });
    pumpRenderQueue(session);
  }

  function enqueueSnapshot(session, data) {
    session.renderRevision = (session.renderRevision || 0) + 1;
    cancelAnimationFrame(session.outputFrame);
    session.outputFrame = 0;
    session.outputParts.length = 0;
    // A canonical snapshot includes every earlier runtime byte. Discard only
    // queued (not-yet-rendered) pre-snapshot operations; output arriving after
    // this message is appended behind the snapshot by the WebSocket event loop.
    session.renderQueue.length = 0;
    session.decoder = new TextDecoder();
    session.renderQueue.push({ kind: "snapshot", data });
    pumpRenderQueue(session);
  }

  function pumpRenderQueue(session) {
    if (session.disposed) return;
    if (session.renderBusy || !session.renderQueue.length) return;
    const operation = session.renderQueue.shift();
    session.renderBusy = true;
    if (operation.kind === "snapshot") {
      session.term.reset();
      session.mouseControl.requested.clear();
      session.unread = false;
      const finishSnapshot = () => {
        session.renderBusy = false;
        updateTerminalUnread(session);
        updateMouseModeButton();
        const firstNativeSnapshot = session.terminal.backend === "native-pty" && !session.initialSnapshotRendered;
        session.initialSnapshotRendered = true;
        if (firstNativeSnapshot && state.activeId === session.id) {
          requestAnimationFrame(() => {
            fitSession(session);
            focusTerminal(session);
          });
        }
        pumpRenderQueue(session);
      };
      if (operation.data) session.term.write(operation.data, finishSnapshot);
      else finishSnapshot();
      return;
    }

    const buffer = session.term.buffer.active;
    const wasAtBottom = buffer.viewportY >= buffer.baseY;
    const anchorLine = buffer.viewportY;
    const renderRevision = session.renderRevision || 0;
    const inactive = state.activeId !== session.id;

    session.term.write(operation.data, () => {
      if (session.disposed) return;
      const current = session.term.buffer.active;
      // A resize/snapshot can replace the buffer while xterm parses a queued
      // write. Only restore a valid anchor from the same render generation.
      if (!wasAtBottom && renderRevision === (session.renderRevision || 0)
          && current.type === "normal" && Number.isFinite(anchorLine) && Number.isFinite(current.baseY)) {
        session.term.scrollToLine(Math.max(0, Math.floor(Math.min(anchorLine, current.baseY))));
      }
      if (!wasAtBottom || inactive) session.unread = true;
      else if (isAtBottom(session)) session.unread = false;
      updateTerminalUnread(session);
      session.renderBusy = false;
      pumpRenderQueue(session);
    });
  }

  function applyServerResize(session, cols, rows) {
    if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 2 || rows < 2) return;
    session.lastDimensions = `${cols}x${rows}`;
    if (session.term.cols === cols && session.term.rows === rows) return;
    session.applyingServerResize = true;
    try {
      session.term.resize(cols, rows);
    } finally {
      session.applyingServerResize = false;
    }
  }

  function sendTerminalInput(session, data) {
    if (!data) return;
    if (!session.socket || session.socket.readyState !== WebSocket.OPEN) {
      showToast("Terminal is reconnecting; input was not sent.");
      connectSession(session);
      return;
    }
    session.socket.send(JSON.stringify({ type: "input", data }));
  }

  function sendTerminalResize(session, cols, rows) {
    if (!Number.isFinite(cols) || !Number.isFinite(rows) || cols < 2 || rows < 2) return;
    if (session.terminal.backend === "native-pty" && !session.initialSnapshotRendered) return;
    const dimensions = `${cols}x${rows}`;
    if (session.lastDimensions === dimensions) return;
    if (!session.socket || session.socket.readyState !== WebSocket.OPEN) return;
    session.lastDimensions = dimensions;
    session.socket.send(JSON.stringify({ type: "resize", cols, rows }));
  }

  function fitActiveTerminal() {
    const session = state.sessions.get(state.activeId);
    if (session) requestAnimationFrame(() => fitSession(session));
  }

  function fitSession(session) {
    if (session.surface.hidden || !session.surface.isConnected) return;
    try {
      if (session.terminal.backend === "native-pty") {
        const size = session.fitAddon.proposeDimensions();
        if (size && Number.isFinite(size.cols) && Number.isFinite(size.rows)) {
          session.term.resize(Math.max(2, Math.min(512, Math.floor(size.cols))), Math.max(2, Math.min(256, Math.floor(size.rows))));
        }
      } else {
        session.fitAddon.fit();
      }
      sendTerminalResize(session, session.term.cols, session.term.rows);
    } catch (_) {
      // A transient zero-size layout during rotation will trigger another ResizeObserver pass.
    }
  }

  function focusTerminal(session) {
    const textarea = session.surface.querySelector(".xterm-helper-textarea");
    if (textarea) textarea.focus({ preventScroll: true });
    else session.term.focus();
  }

  function handleTerminalShortcut(event, session) {
    if (event.type !== "keydown") return true;
    const key = event.key.toLowerCase();
    const copyShortcut = (event.ctrlKey && event.shiftKey && key === "c") || (event.metaKey && key === "c") || (event.ctrlKey && key === "c" && session.term.hasSelection());
    const pasteShortcut = (event.ctrlKey && event.shiftKey && key === "v") || (event.metaKey && key === "v");
    if (copyShortcut) {
      copySelection(session);
      return false;
    }
    if (pasteShortcut) {
      pasteClipboard(session);
      return false;
    }
    return true;
  }

  function handleTerminalWheel(event, session) {
    if (!event.deltaY || session.terminal.status !== "running") return true;
    if (session.terminal.backend === "native-pty") {
      // xterm reports the wheel to apps that enabled mouse tracking, sends arrow
      // keys on the alternate screen otherwise, and scrolls its own history on
      // the normal screen — the same as a desktop terminal.
      return true;
    }
    event.preventDefault();
    if (!session.socket || session.socket.readyState !== WebSocket.OPEN) {
      connectSession(session);
      return false;
    }

    const screen = session.surface.querySelector(".xterm-screen") || session.surface;
    const rect = screen.getBoundingClientRect();
    const relativeX = rect.width > 0 ? (event.clientX - rect.left) / rect.width : 0;
    const relativeY = rect.height > 0 ? (event.clientY - rect.top) / rect.height : 0;
    const column = Math.max(1, Math.min(session.term.cols, Math.floor(relativeX * session.term.cols) + 1));
    const row = Math.max(1, Math.min(session.term.rows, Math.floor(relativeY * session.term.rows) + 1));
    const button = event.deltaY < 0 ? 64 : 65;
    const steps = Math.max(1, Math.min(8, Math.ceil(Math.abs(event.deltaY) / 100)));
    const sequence = `\u001b[<${button};${column};${row}M`.repeat(steps);
    session.socket.send(JSON.stringify({ type: "input", data: sequence }));
    return false;
  }

  async function copySelection(session) {
    const selected = session.term.getSelection();
    if (!selected) return;
    try {
      await navigator.clipboard.writeText(selected);
      showToast("Selection copied.");
    } catch (_) {
      showToast("Clipboard access was denied.");
    }
  }

  async function pasteClipboard(session = state.sessions.get(state.activeId)) {
    if (!session) return;
    try {
      const text = await navigator.clipboard.readText();
      if (text) session.term.paste(text);
      focusTerminal(session);
    } catch (_) {
      showToast("Allow clipboard access to paste.");
      focusTerminal(session);
    }
  }

  function handleAccessoryKey(event) {
    const button = event.target.closest("button");
    if (!button) return;
    const session = state.sessions.get(state.activeId);
    if (!session) {
      showToast("Select a terminal first.");
      return;
    }
    if (button === elements.ctrlKey) {
      setStickyControl(!state.ctrlPending);
      focusTerminal(session);
      return;
    }
    if (button === elements.copyKey) {
      copySelection(session);
      focusTerminal(session);
      return;
    }
    if (button === elements.mouseModeKey) {
      setApplicationMouseMode(session, !session.mouseControl.appMode);
      focusTerminal(session);
      return;
    }
    if (button === elements.pasteKey) {
      pasteClipboard(session);
      return;
    }
    const key = TERMINAL_KEYS[button.dataset.terminalKey];
    if (key) sendTerminalInput(session, applyStickyControl(key));
    focusTerminal(session);
  }

  function preserveTerminalFocus(event) {
    if (event.pointerType === "mouse" && event.target.closest("button") && state.activeId) {
      event.preventDefault();
    }
  }

  function applyStickyControl(data) {
    if (!state.ctrlPending) return data;
    setStickyControl(false);
    if (data.length !== 1 || data.charCodeAt(0) < 32) return data;
    const code = data.toUpperCase().charCodeAt(0);
    if (code >= 64 && code <= 95) return String.fromCharCode(code & 31);
    return data;
  }

  function setStickyControl(enabled) {
    state.ctrlPending = enabled;
    elements.ctrlKey.setAttribute("aria-pressed", String(enabled));
  }

  function isAtBottom(session) {
    const buffer = session.term.buffer.active;
    return buffer.viewportY >= buffer.baseY;
  }

  function followLatestOutput() {
    const session = state.sessions.get(state.activeId);
    if (!session) return;
    session.term.scrollToBottom();
    session.unread = false;
    updateTerminalUnread(session);
    focusTerminal(session);
  }

  function updateTerminalUnread(session) {
    const node = elements.workspaceList.querySelector(`.terminal-row[data-key="${cssEscape(session.id)}"]`);
    if (node) {
      node.classList.toggle("has-unread", session.unread);
      const button = node.querySelector(".terminal-select");
      if (button) button.setAttribute("aria-label", terminalAccessibleName(session.terminal, session.unread));
    }
    if (state.activeId === session.id) updateNewOutputButton();
    renderTerminalTabs();
  }

  function updateNewOutputButton() {
    const session = state.sessions.get(state.activeId);
    elements.newOutputButton.hidden = !session?.unread;
    if (session?.unread) elements.terminalAnnouncer.textContent = `New output in ${session.terminal.name}.`;
  }

  function updateActiveContext() {
    const terminal = state.terminals.get(state.activeId);
    const workspace = terminal?.workspace || state.workspaces.find((item) => item.id === state.activeWorkspaceId);
    const workspaceId = workspace?.id || null;
    const workspacePath = workspace?.path || "No workspace selected";
    const pathChanged = elements.activeWorkspace.textContent !== workspacePath;
    const workspaceChanged = state.activePathWorkspaceId !== workspaceId;
    if (pathChanged) elements.activeWorkspace.textContent = workspacePath;
    elements.activeWorkspace.title = workspace
      ? `${workspace.name}: ${workspacePath}`
      : workspacePath;
    elements.activeWorkspace.setAttribute("aria-label", workspace
      ? `Active workspace path: ${workspacePath}`
      : workspacePath);
    if (workspaceChanged || pathChanged) {
      state.activePathWorkspaceId = workspaceId;
      state.activePathStickToEnd = true;
      scrollActivePathToEnd(true);
    }
    elements.activeTerminal.textContent = terminal?.name || "Select a terminal";
    elements.activeBackend.hidden = terminal?.backend !== "legacy-tmux";
    elements.activeBackend.textContent = terminal?.backend === "legacy-tmux" ? "legacy tmux" : "";
    elements.activeBackend.title = terminal ? `Backend: ${terminalBackendLabel(terminal)}` : "";
    document.title = terminal
      ? `${terminal.name}${terminal.backend === "legacy-tmux" ? " (legacy tmux)" : ""} — webterm`
      : "webterm";
    updateConnectionStatus();
  }

  function trackActivePathScroll() {
    if (state.activePathScrollFrame) return;
    const path = elements.activeWorkspace;
    const resized = state.activePathClientWidth !== path.clientWidth;
    state.activePathClientWidth = path.clientWidth;
    // Resize can clamp scrollLeft and emit scroll before the resize callback.
    // That is not a user choosing to leave the end of the workspace path.
    if (resized && state.activePathStickToEnd) {
      scrollActivePathToEnd(false);
      return;
    }
    state.activePathStickToEnd = path.scrollWidth - path.clientWidth - path.scrollLeft <= 2;
  }

  function scrollActivePathToEnd(force) {
    if (!force && !state.activePathStickToEnd) return;
    cancelAnimationFrame(state.activePathScrollFrame);
    state.activePathScrollFrame = requestAnimationFrame(() => {
      state.activePathScrollFrame = 0;
      elements.activeWorkspace.scrollLeft = elements.activeWorkspace.scrollWidth;
      state.activePathClientWidth = elements.activeWorkspace.clientWidth;
      state.activePathStickToEnd = true;
    });
  }

  function updateConnectionStatus() {
    const session = state.sessions.get(state.activeId);
    const status = !navigator.onLine ? "offline" : session?.connection || "idle";
    const labels = { online: "Connected", connecting: "Connecting", offline: "Reconnecting", stopped: "Stopped", idle: "Idle" };
    elements.connectionChip.className = `connection-chip is-${status}`;
    elements.connectionLabel.textContent = labels[status] || "Idle";
  }

  function mobileNavigation() {
    return window.matchMedia("(max-width: 760px), (pointer: coarse)").matches;
  }

  function toggleNavigation() {
    if (mobileNavigation()) {
      setMobileDrawer(!elements.appView.classList.contains("drawer-open"));
      return;
    }
    const hidden = elements.appView.classList.toggle("sidebar-hidden");
    elements.drawerOpen.setAttribute("aria-expanded", String(!hidden));
    fitActiveTerminal();
  }

  function openNavigation() {
    if (mobileNavigation()) setMobileDrawer(true);
    else {
      elements.appView.classList.remove("sidebar-hidden");
      elements.drawerOpen.setAttribute("aria-expanded", "true");
      elements.sidebar.querySelector("button:not([hidden])")?.focus({ preventScroll: true });
      fitActiveTerminal();
    }
  }

  function closeMobileDrawer() {
    if (mobileNavigation()) setMobileDrawer(false);
  }

  function setMobileDrawer(open) {
    elements.appView.classList.toggle("drawer-open", open);
    elements.drawerOpen.setAttribute("aria-expanded", String(open));
    if (open) requestAnimationFrame(() => elements.sidebar.querySelector("button:not([hidden])")?.focus({ preventScroll: true }));
    else if (state.activeId) requestAnimationFrame(() => focusTerminal(state.sessions.get(state.activeId)));
  }

  function openDialog(kind, id, trigger) {
    const terminal = state.terminals.get(String(id));
    const workspace = state.workspaces.find((item) => item.id === String(id)) || terminal?.workspace;
    const configs = {
      "workspace-create": {
        subject: "Workspace",
        title: "Add workspace",
        copy: "Choose an allowed folder on this server, then give it a workspace name.",
        submit: "Add workspace",
        fields: [],
      },
      "workspace-rename": {
        subject: "Workspace",
        title: "Rename workspace",
        copy: `Change the display name for ${workspace?.name || "this workspace"}.`,
        submit: "Save name",
        fields: [field("name", "Name", "", workspace?.name)],
      },
      "workspace-delete": {
        subject: "Workspace",
        title: "Delete workspace?",
        copy: `This removes ${workspace?.name || "the workspace"} from webterm. Its folder is not deleted.`,
        submit: "Delete workspace",
        danger: true,
        fields: [],
      },
      "terminal-rename": {
        subject: terminal?.workspace.name || "Terminal",
        title: "Rename terminal",
        copy: `Change the name for ${terminal?.name || "this terminal"}.`,
        submit: "Save name",
        fields: [field("name", "Name", "", terminal?.name)],
      },
      "terminal-delete": {
        subject: terminal?.workspace.name || "Terminal",
        title: "Delete terminal?",
        copy: `This stops and removes ${terminal?.name || "the terminal"} and its retained session.`,
        submit: "Delete terminal",
        danger: true,
        fields: [],
      },
    };
    const config = configs[kind];
    if (!config) return;

    state.dialog = { kind, id: String(id ?? ""), workspace, terminal, config };
    if (kind === "workspace-create") {
      state.dialog.picker = { current: "", currentName: "", selected: "", request: 0, nameDirty: false };
    }
    state.dialogTrigger = trigger || document.activeElement;
    elements.dialogEyebrow.textContent = config.subject;
    elements.dialogTitle.textContent = config.title;
    elements.dialogCopy.textContent = config.copy;
    elements.dialogSubmit.textContent = config.submit;
    elements.dialogSubmit.disabled = false;
    elements.dialogSubmit.classList.toggle("is-danger", Boolean(config.danger));
    elements.actionDialog.classList.toggle("is-folder-picker", kind === "workspace-create");
    elements.dialogError.textContent = "";
    elements.dialogFields.replaceChildren(...config.fields.map(createDialogField));
    if (kind === "workspace-create") elements.dialogFields.replaceChildren(createWorkspacePicker());
    elements.actionDialog.showModal();
    if (kind === "workspace-create") {
      elements.dialogSubmit.disabled = true;
      loadPickerFolder(null, true);
    }
    requestAnimationFrame(() => elements.dialogFields.querySelector("input:not([type=hidden])")?.focus({ preventScroll: true }) || elements.dialogSubmit.focus({ preventScroll: true }));
  }

  function field(name, label, placeholder = "", value = "") {
    return { name, label, placeholder, value: value || "" };
  }

  function createDialogField(config) {
    const wrapper = make("div", "dialog-field");
    const inputId = `dialog-${config.name}`;
    const label = make("label", "", config.label);
    label.htmlFor = inputId;
    const input = document.createElement("input");
    input.id = inputId;
    input.name = config.name;
    input.required = true;
    input.autocomplete = "off";
    input.placeholder = config.placeholder;
    input.value = config.value;
    wrapper.append(label, input);
    return wrapper;
  }

  function createWorkspacePicker() {
    const picker = make("section", "folder-picker");
    picker.id = "folder-picker";

    const selected = make("div", "folder-selected-card");
    const selectedLabel = make("span", "folder-card-label", "Selected folder");
    const selectedPath = make("code", "folder-card-path", "Loading…");
    selectedPath.id = "folder-selected";
    selectedPath.title = "Selected folder";
    selected.append(selectedLabel, selectedPath);

    const nameField = make("div", "dialog-field");
    const nameLabel = make("label", "", "Workspace name");
    nameLabel.htmlFor = "dialog-name";
    const nameInput = document.createElement("input");
    nameInput.id = "dialog-name";
    nameInput.name = "name";
    nameInput.required = true;
    nameInput.autocomplete = "off";
    nameInput.addEventListener("input", () => {
      if (state.dialog?.picker) state.dialog.picker.nameDirty = true;
    });
    nameField.append(nameLabel, nameInput);

    const pathInput = document.createElement("input");
    pathInput.type = "hidden";
    pathInput.name = "path";
    pathInput.id = "folder-path";

    const browser = make("div", "folder-browser");
    const browserHeader = make("div", "folder-browser-header");
    const currentWrap = make("div", "folder-current-wrap");
    const currentLabel = make("span", "folder-card-label", "Browsing");
    const currentPath = make("code", "folder-current", "Loading folders…");
    currentPath.id = "folder-current";
    currentWrap.append(currentLabel, currentPath);
    const selectButton = make("button", "folder-select-current", "Use this folder");
    selectButton.id = "folder-select-current";
    selectButton.type = "button";
    selectButton.disabled = true;
    selectButton.addEventListener("click", selectCurrentPickerFolder);
    browserHeader.append(currentWrap, selectButton);

    const list = make("div", "folder-list");
    list.id = "folder-list";
    list.setAttribute("role", "list");
    list.setAttribute("aria-label", "Folders");
    list.addEventListener("click", handleFolderClick);
    list.addEventListener("keydown", handleFolderKeys);
    browser.append(browserHeader, list);
    picker.append(selected, nameField, pathInput, browser);
    return picker;
  }

  async function loadPickerFolder(path, initial = false) {
    const dialog = state.dialog;
    if (!dialog?.picker || dialog.kind !== "workspace-create") return;
    const request = ++dialog.picker.request;
    const list = document.getElementById("folder-list");
    const selectButton = document.getElementById("folder-select-current");
    list?.setAttribute("aria-busy", "true");
    if (list) list.replaceChildren(make("p", "folder-state", "Loading folders…"));
    if (selectButton) selectButton.disabled = true;
    elements.dialogError.textContent = "";
    try {
      const query = path ? `?path=${encodeURIComponent(path)}` : "";
      const { body } = await apiFetch(`/folders${query}`, { method: "GET" });
      if (state.dialog !== dialog || dialog.picker.request !== request) return;
      dialog.picker.current = String(body.current || "");
      dialog.picker.currentName = String(body.selected_name || "workspace");
      renderPickerListing(body);
      if (initial) selectCurrentPickerFolder();
    } catch (error) {
      if (state.dialog !== dialog || dialog.picker.request !== request) return;
      renderPickerError(friendlyError(error, "Unable to browse this folder."));
    }
  }

  function renderPickerListing(body) {
    const current = document.getElementById("folder-current");
    const list = document.getElementById("folder-list");
    const selectButton = document.getElementById("folder-select-current");
    if (!current || !list || !selectButton) return;
    current.textContent = body.current;
    current.title = body.current;
    const rows = [];
    if (body.parent) rows.push(folderRow(body.parent, "Up one folder", "up"));
    for (const entry of body.entries || []) rows.push(folderRow(entry.path, entry.name, "folder"));
    if (!rows.length) rows.push(make("p", "folder-state", "No folders inside this location."));
    if (body.truncated) rows.push(make("p", "folder-state", "Showing the first 500 folders."));
    list.replaceChildren(...rows);
    list.removeAttribute("aria-busy");
    selectButton.disabled = false;
  }

  function folderRow(path, label, kind) {
    const button = make("button", `folder-row is-${kind}`);
    button.type = "button";
    button.dataset.path = path;
    button.setAttribute("aria-label", kind === "up" ? "Up to parent folder" : `Open folder ${label}`);
    const icon = make("span", "folder-row-icon", kind === "up" ? "↰" : "▱");
    icon.setAttribute("aria-hidden", "true");
    button.append(icon, make("span", "folder-row-name", label), make("span", "folder-row-chevron", "›"));
    return button;
  }

  function handleFolderClick(event) {
    const row = event.target.closest(".folder-row[data-path]");
    if (row) loadPickerFolder(row.dataset.path);
  }

  function handleFolderKeys(event) {
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    const rows = Array.from(event.currentTarget.querySelectorAll(".folder-row"));
    if (!rows.length) return;
    event.preventDefault();
    const index = rows.indexOf(document.activeElement);
    const next = event.key === "Home" ? 0
      : event.key === "End" ? rows.length - 1
        : event.key === "ArrowDown" ? Math.min(rows.length - 1, index + 1)
          : Math.max(0, index < 0 ? 0 : index - 1);
    rows[next].focus({ preventScroll: true });
  }

  function selectCurrentPickerFolder() {
    const picker = state.dialog?.picker;
    if (!picker?.current) return;
    picker.selected = picker.current;
    const selected = document.getElementById("folder-selected");
    const pathInput = document.getElementById("folder-path");
    const nameInput = document.getElementById("dialog-name");
    if (selected) {
      selected.textContent = picker.selected;
      selected.title = picker.selected;
    }
    if (pathInput) pathInput.value = picker.selected;
    if (nameInput && (!picker.nameDirty || !nameInput.value)) nameInput.value = picker.currentName;
    elements.dialogSubmit.disabled = false;
  }

  function renderPickerError(message) {
    const list = document.getElementById("folder-list");
    const currentPath = state.dialog?.picker?.current || null;
    if (!list) return;
    const copy = make("p", "folder-state is-error", message);
    const retry = make("button", "secondary-button folder-retry", "Try again");
    retry.type = "button";
    retry.addEventListener("click", () => loadPickerFolder(currentPath));
    list.replaceChildren(copy, retry);
    list.removeAttribute("aria-busy");
  }

  function closeDialog() {
    if (elements.actionDialog.open) elements.actionDialog.close();
    state.dialog = null;
    const trigger = state.dialogTrigger;
    state.dialogTrigger = null;
    trigger?.focus?.({ preventScroll: true });
  }

  async function submitDialog(event) {
    event.preventDefault();
    if (!state.dialog) return;
    const { kind, id, workspace, terminal } = state.dialog;
    const formData = new FormData(elements.actionForm);
    const values = Object.fromEntries(formData.entries());
    let path;
    let options;

    if (kind === "workspace-create") {
      path = "/workspaces";
      options = { method: "POST", json: { name: values.name, path: values.path } };
    } else if (kind === "workspace-rename") {
      path = `/workspaces/${encodeURIComponent(id)}?force=true`;
      options = { method: "PATCH", json: { name: values.name } };
    } else if (kind === "workspace-delete") {
      path = `/workspaces/${encodeURIComponent(id)}?force=true`;
      options = { method: "DELETE" };
    } else if (kind === "terminal-rename") {
      path = `/terminals/${encodeURIComponent(id)}`;
      options = { method: "PATCH", json: { name: values.name } };
    } else if (kind === "terminal-delete") {
      path = `/terminals/${encodeURIComponent(id)}`;
      options = { method: "DELETE" };
    }

    setButtonBusy(elements.dialogSubmit, true, "Working…");
    elements.dialogError.textContent = "";
    try {
      const { body } = await apiFetch(path, options);
      if (kind === "workspace-create" && body?.workspace?.id !== undefined) {
        state.activeId = null;
        state.activeWorkspaceId = String(body.workspace.id);
        state.ensureAttempted.delete(state.activeWorkspaceId);
      }
      if (kind === "terminal-delete" && terminal) removeTerminalSession(terminal.id);
      if (kind === "workspace-delete" && workspace) {
        for (const item of workspace.terminals) removeTerminalSession(item.id);
      }
      closeDialog();
      await refreshWorkspaces(false);
      if (kind === "workspace-create" && state.activeWorkspaceId) {
        enterWorkspace(state.activeWorkspaceId, true);
      }
    } catch (error) {
      if (error.status !== 401) elements.dialogError.textContent = friendlyError(error, "The change could not be saved.");
    } finally {
      setButtonBusy(elements.dialogSubmit, false);
    }
  }

  function removeTerminalSession(id) {
    const session = state.sessions.get(String(id));
    if (!session) return;
    disposeSession(session);
    session.surface.remove();
    state.sessions.delete(String(id));
    if (state.activeId === String(id)) deactivateTerminal();
  }

  function disposeSession(session) {
    session.disposed = true;
    session.desired = false;
    session.generation += 1;
    clearTimeout(session.reconnectTimer);
    if (session.outputFrame) cancelAnimationFrame(session.outputFrame);
    session.outputParts.length = 0;
    session.renderQueue.length = 0;
    try { session.socket?.close(1000, "client closed"); } catch (_) { /* already closed */ }
    try { session.touchCleanup?.(); } catch (_) { /* already removed */ }
    try { session.linkCleanup?.(); } catch (_) { /* already removed */ }
    window.WebTermLinks.hide();
    try { session.term.dispose(); } catch (_) { /* already disposed */ }
  }

  function markSessionStopped(session) {
    session.desired = false;
    clearTimeout(session.reconnectTimer);
    session.reconnectTimer = 0;
    if (session.socket) {
      session.generation += 1;
      try { session.socket.close(1000, "terminal stopped"); } catch (_) { /* already closed */ }
      session.socket = null;
    }
    session.connection = "stopped";
    session.stoppedOverlay.hidden = false;
    if (state.activeId === session.id) updateConnectionStatus();
  }

  function mergeCapabilities(capabilities) {
    if (!capabilities) return;
    if (Array.isArray(capabilities)) {
      for (const name of capabilities) state.capabilities[String(name)] = true;
      return;
    }
    state.capabilities = deepMerge(state.capabilities, capabilities);
  }

  function can(resource, operation) {
    const caps = state.capabilities || {};
    if (caps[`${resource}_crud`] === true) return true;
    const aliases = [
      `${resource}.${operation}`,
      `${resource}:${operation}`,
      `${resource}_${operation}`,
      `${operation}_${resource}`,
      `${resource}${operation[0].toUpperCase()}${operation.slice(1)}`,
    ];
    if (operation === "rename") aliases.push(`${resource}.update`, `${resource}:update`, `${resource}_update`);
    for (const key of aliases) if (caps[key] === true) return true;
    const plural = `${resource}s`;
    const nested = caps[resource] || caps[plural];
    if (nested && typeof nested === "object" && (nested[operation] === true || (operation === "rename" && nested.update === true))) return true;
    return false;
  }

  function updateCapabilityControls() {
    elements.workspaceCreate.hidden = !can("workspace", "create");
  }

  async function apiFetch(path, options = {}, allowUnauthorized = false) {
    const headers = new Headers(options.headers || {});
    headers.set("Accept", "application/json");
    headers.set("X-WebTerm-Control", "1");
    let body = options.body;
    if (options.json !== undefined) {
      headers.set("Content-Type", "application/json");
      body = JSON.stringify(options.json);
    }
    const method = String(options.method || "GET").toUpperCase();
    if (!["GET", "HEAD", "OPTIONS"].includes(method) && state.csrf) headers.set("X-CSRF-Token", state.csrf);

    const response = await fetch(`${API_ROOT}${path}`, {
      ...options,
      method,
      headers,
      body,
      credentials: "same-origin",
      cache: "no-store",
    });
    const bodyValue = await readJson(response);
    if (!response.ok) {
      const error = responseErrorFromBody(response, bodyValue);
      if (response.status === 401 && !allowUnauthorized) {
        clearAuthenticatedState();
        showLogin("Your session expired. Sign in again.");
      }
      throw error;
    }
    return { response, body: bodyValue };
  }

  function rememberSecurityContext(response, body) {
    const value = response.headers.get("X-CSRF-Token") || body?.csrf_token || body?.csrfToken || body?.csrf || "";
    if (value) state.csrf = String(value);
    mergeCapabilities(body?.capabilities);
  }

  async function readJson(response) {
    if (response.status === 204) return {};
    const text = await response.text();
    if (!text) return {};
    try { return JSON.parse(text); } catch (_) { return { message: text }; }
  }

  async function responseError(response) {
    return responseErrorFromBody(response, await readJson(response));
  }

  function responseErrorFromBody(response, body) {
    const error = new Error(body?.error || body?.message || `Request failed (${response.status})`);
    error.status = response.status;
    return error;
  }

  function friendlyError(error, fallback) {
    if (!error) return fallback;
    if (error.status === 401) return "Incorrect password or expired session.";
    if (error.status === 429) return "Too many attempts. Wait a moment and try again.";
    if (error.status >= 400 && error.status < 500 && error.message) return error.message;
    return fallback;
  }

  function setButtonBusy(button, busy, label = "Working…") {
    if (busy) {
      button.dataset.label = button.textContent;
      button.textContent = label;
      button.disabled = true;
    } else {
      button.textContent = button.dataset.label || button.textContent;
      button.disabled = false;
      delete button.dataset.label;
    }
  }

  function showToast(message) {
    clearTimeout(state.toastTimer);
    elements.toast.textContent = message;
    elements.toast.hidden = false;
    state.toastTimer = window.setTimeout(() => { elements.toast.hidden = true; }, 3000);
  }

  function make(tag, className = "", text = "") {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function deepMerge(target, source) {
    const result = { ...target };
    for (const [key, value] of Object.entries(source || {})) {
      result[key] = value && typeof value === "object" && !Array.isArray(value)
        ? deepMerge(result[key] && typeof result[key] === "object" ? result[key] : {}, value)
        : value;
    }
    return result;
  }

  function cssEscape(value) {
    return window.CSS?.escape ? window.CSS.escape(String(value)) : String(value).replace(/[^a-zA-Z0-9_-]/g, "\\$&");
  }

  function safeClass(value) {
    return String(value).replace(/[^a-z0-9_-]/gi, "-").toLowerCase();
  }

  function toCamel(value) {
    return value.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
  }
})();
