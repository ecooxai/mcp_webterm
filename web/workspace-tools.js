(() => {
  "use strict";
  // Workspace row tools: a run panel (listening ports + processes), a webview tab
  // that frames a port through WebTerm's proxy, and a git changes/diff panel.
  const RUN_REFRESH_MS = 3000;
  const WEBVIEW_REFRESH_MS = 5000;
  const DIFF_LINE_LIMIT = 20000;
  const GIT_ICON = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><circle cx="4.5" cy="3.5" r="1.6"/><circle cx="4.5" cy="12.5" r="1.6"/><circle cx="11.5" cy="5" r="1.6"/><path d="M4.5 5.1v5.8M11.5 6.6c0 2.8-7 2-7 4.3"/></svg>';
  const RUN_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3.2v9.6L12.6 8z" fill="currentColor"/></svg>';

  let ctx, panel = null, menu = null;
  let webview = null, webviewTab = null, webviewActive = false;

  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const button = (className, text, label, onClick) => {
    const node = el("button", className, text);
    node.type = "button";
    if (label) {
      node.title = label;
      node.setAttribute("aria-label", label);
    }
    if (onClick) node.addEventListener("click", onClick);
    return node;
  };
  const bytes = (value) => {
    if (!Number.isFinite(value)) return "";
    const units = ["B", "KB", "MB", "GB"];
    let n = value, unit = 0;
    while (n >= 1024 && unit < units.length - 1) { n /= 1024; unit += 1; }
    return `${n.toFixed(n >= 10 || unit === 0 ? 0 : 1)} ${units[unit]}`;
  };
  const query = (params) => new URLSearchParams(params).toString();
  const proxyUrl = (port) => `/proxy/${port}/`;

  function init(context) {
    ctx = context;
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (menu && !menu.contains(target)) closeMenu();
      if (!panel || panel.node.contains(target)) return;
      // The trigger toggles its own panel on click.
      if (target.closest?.(`.workspace-action[data-action="${panel.kind}"][data-id="${panel.workspaceId}"]`)) return;
        closePanel();
    });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      if (menu) closeMenu();
      else if (panel) closePanel(true);
    });
    window.addEventListener("resize", () => panel && placePanel());
    buildWebview();
  }

  function icon(action) {
    return action === "workspace-git" ? GIT_ICON : RUN_ICON;
  }

  // ---------- floating panel ----------

  function toggle(kind, workspace, trigger) {
    if (panel && panel.kind === kind && panel.workspaceId === workspace.id) {
      closePanel();
      return;
    }
    closePanel();
    const node = el("aside", `tools-panel is-${kind === "workspace-git" ? "git" : "run"}`);
    node.setAttribute("role", "dialog");
    node.setAttribute("aria-label", kind === "workspace-git" ? `Git changes in ${workspace.name}` : `Running in ${workspace.name}`);
    document.body.append(node);
    panel = { kind, workspaceId: workspace.id, workspace, trigger, node, timer: 0, seq: 0 };
    trigger?.setAttribute("aria-expanded", "true");
    if (kind === "workspace-git") openGit(panel);
    else openRun(panel);
    placePanel();
  }

  function closePanel(restoreFocus = false) {
    if (!panel) return;
    clearInterval(panel.timer);
    panel.trigger?.setAttribute("aria-expanded", "false");
    panel.node.remove();
    if (restoreFocus) panel.trigger?.focus?.();
    panel = null;
  }

  function placePanel() {
    if (!panel) return;
    const node = panel.node;
    const vw = window.innerWidth, vh = window.innerHeight;
    if (vw <= 760) {
      Object.assign(node.style, { left: "8px", right: "8px", top: "56px", bottom: "8px", width: "", height: "", maxHeight: "" });
      return;
    }
    const sidebar = ctx.elements.sidebar?.getBoundingClientRect();
    const row = (panel.trigger?.closest(".workspace-row") || panel.trigger)?.getBoundingClientRect();
    const left = Math.round(Math.max(8, sidebar?.right ? sidebar.right + 6 : (row?.right || 8) + 6));
    const git = panel.kind === "workspace-git";
    const width = Math.max(300, Math.min(git ? 1040 : 480, vw - left - 12));
    const height = Math.min(vh - 24, git ? 760 : 620);
    const top = Math.round(Math.max(12, Math.min(vh - height - 12, (row?.top || 60) - 8)));
    Object.assign(node.style, {
      left: `${Math.min(left, vw - width - 8)}px`, right: "", bottom: "",
      top: `${top}px`, width: `${width}px`,
      height: git ? `${height}px` : "", maxHeight: `${height}px`,
    });
  }

  function panelHeader(title, subtitle, onRefresh) {
    const header = el("header", "tools-header");
    const heading = el("div", "tools-heading");
    const name = el("div", "tools-title");
    if (typeof title === "string") name.textContent = title;
    else name.append(...title);
    heading.append(name);
    const sub = el("div", "tools-subtitle", subtitle || "");
    heading.append(sub);
    const actions = el("div", "tools-header-actions");
    if (onRefresh) actions.append(button("icon-button tools-refresh", "⟳", "Refresh", onRefresh));
    actions.append(button("icon-button", "×", "Close", () => closePanel(true)));
    header.append(heading, actions);
    return { header, name, sub };
  }

  // ---------- run panel ----------

  function openRun(state) {
    const { header, sub } = panelHeader(`Running in ${state.workspace.name}`, state.workspace.path, () => loadRun(state));
    const portsTitle = el("h3", "tools-section-title", "Ports");
    const ports = el("div", "tools-ports");
    const procTitle = el("h3", "tools-section-title", "Processes");
    const processes = el("ul", "tools-process-list");
    const body = el("div", "tools-body");
    body.append(portsTitle, ports, procTitle, processes);
    state.node.append(header, body);
    Object.assign(state, { sub, ports, processes, portsTitle, procTitle });
    ports.append(el("p", "tools-empty", "Loading…"));
    loadRun(state);
    state.timer = setInterval(() => { if (!document.hidden) loadRun(state); }, RUN_REFRESH_MS);
  }

  async function activity(workspaceId) {
    const { body } = await ctx.request(`/workspace-activity?${query({ workspace_id: workspaceId })}`, { method: "GET" });
    return body || { ports: [], processes: [] };
  }

  async function loadRun(state) {
    const id = ++state.seq;
    let data;
    try {
      data = await activity(state.workspaceId);
    } catch (error) {
      if (panel !== state || id !== state.seq) return;
      state.ports.replaceChildren(el("p", "tools-empty is-error", error.message || "Activity unavailable"));
      return;
    }
    if (panel !== state || id !== state.seq) return;
    state.portsTitle.textContent = `Ports · ${data.ports.length}`;
    state.procTitle.textContent = `Processes · ${data.processes.length}`;
    state.ports.replaceChildren(...(data.ports.length
      ? data.ports.map((port) => portChip(port, () => {
        closePanel();
        openWebview(state.workspace, port.port);
      }))
      : [el("p", "tools-empty", "No listening ports")]));
    state.processes.replaceChildren(...(data.processes.length
      ? data.processes.map(processRow)
      : [el("li", "tools-empty", "No processes in this workspace")]));
  }

  function portChip(port, onClick, active = false) {
    const chip = button("tools-port", "", `Open port ${port.port}${port.name ? ` (${port.name})` : ""} in the webview`, onClick);
    chip.append(el("span", "tools-port-number", `:${port.port}`));
    if (port.name) chip.append(el("span", "tools-port-name", port.name));
    chip.setAttribute("aria-pressed", String(active));
    chip.dataset.port = port.port;
    return chip;
  }

  function processRow(proc) {
    const row = el("li", "tools-process");
    const top = el("div", "tools-process-top");
    top.append(el("span", "tools-process-name", proc.name || "?"), el("span", "tools-process-pid", String(proc.pid)));
    for (const port of proc.ports || []) top.append(el("span", "tools-badge", `:${port}`));
    const cmd = el("code", "tools-process-cmd", proc.cmdline || proc.name || "");
    cmd.title = proc.cmdline || "";
    const meta = el("div", "tools-process-meta");
    const where = proc.terminal_name ? `⌨ ${proc.terminal_name}` : "";
    const cwd = proc.relative_cwd !== null && proc.relative_cwd !== undefined ? `./${proc.relative_cwd}` : proc.cwd || "";
    const usage = [
      Number.isFinite(proc.cpu_percent) ? `${proc.cpu_percent.toFixed(1)}% CPU` : "",
      bytes(proc.memory_bytes),
    ].filter(Boolean).join(" · ");
    for (const text of [where, cwd, usage]) if (text) meta.append(el("span", "", text));
    row.append(top, cmd, meta);
    return row;
  }

  // ---------- webview tab ----------

  function buildWebview() {
    webview = el("section", "webview-panel");
    webview.id = "webview-panel";
    webview.hidden = true;
    webview.setAttribute("role", "tabpanel");
    webview.setAttribute("aria-labelledby", "webview-tab");
    const bar = el("div", "webview-toolbar");
    const ports = el("div", "webview-ports");
    const actions = el("div", "webview-actions");
    const reload = button("icon-button", "⟳", "Reload page", () => {
      if (!webview.state) return;
      try { webview.frame.contentWindow.location.reload(); } catch (_) { webview.frame.src = webview.frame.src; }
    });
    const external = el("a", "icon-button webview-external", "↗");
    external.target = "_blank";
    external.rel = "noopener";
    external.title = "Open in new tab";
    external.setAttribute("aria-label", "Open in new tab");
    external.addEventListener("click", () => {
      // Follow in-frame navigation when the page is still same-origin (proxied).
      try { external.href = webview.frame.contentWindow.location.href; } catch (_) { /* keep port root */ }
    });
    const close = button("icon-button", "×", "Close webview", closeWebview);
    actions.append(reload, external, close);
    bar.append(ports, actions);
    const frame = el("iframe", "webview-frame");
    frame.allow = window.WebTermLinks?.IFRAME_ALLOW || "";
    frame.allowFullscreen = true;
    webview.append(bar, frame);
    Object.assign(webview, { ports, frame, external, state: null, timer: 0 });
    ctx.elements.terminalPanel.append(webview);

    webviewTab = button("terminal-tab webview-tab", "", "", showWebview);
    webviewTab.id = "webview-tab";
    webviewTab.dataset.key = "__webview__";
    webviewTab.setAttribute("role", "tab");
    webviewTab.setAttribute("aria-controls", "webview-panel");
    webviewTab.append(el("span", "", "◍"), el("span", "webview-tab-label", "Webview"));
  }

  function openWebview(workspace, port) {
    const state = webview.state && webview.state.workspaceId === workspace.id
      ? webview.state
      : { workspaceId: workspace.id, workspace, ports: [] };
    webview.state = state;
    if (state.port !== port || !webview.frame.getAttribute("src")) {
      state.port = port;
      webview.frame.title = `Port ${port}`;
      webview.frame.src = proxyUrl(port);
    }
    webview.external.href = proxyUrl(port);
    renderWebviewPorts();
    showWebview();
    refreshWebviewPorts();
    ctx.closeDrawer?.();
  }

  function renderWebviewPorts() {
    const state = webview.state;
    if (!state) return;
    const list = state.ports.some((item) => item.port === state.port)
      ? state.ports
      : [{ port: state.port, name: "" }, ...state.ports];
    webview.ports.replaceChildren(
      el("span", "webview-workspace", state.workspace.name),
      ...list.map((port) => portChip(port, () => openWebview(state.workspace, port.port), port.port === state.port)),
    );
    webviewTab.querySelector(".webview-tab-label").textContent = `:${state.port}`;
    webviewTab.title = `Webview · port ${state.port} · ${state.workspace.name}`;
  }

  async function refreshWebviewPorts() {
    const state = webview.state;
    if (!state) return;
    try {
      const data = await activity(state.workspaceId);
      if (webview.state !== state) return;
      state.ports = data.ports;
      renderWebviewPorts();
    } catch (_) { /* keep the last list */ }
  }

  function showWebview() {
    if (!webview?.state) return;
    window.WebTermExplorer?.hideViewer();
    window.WebTermLinks?.hide();
    webviewActive = true;
    webview.hidden = false;
    ctx.elements.terminalPanel.classList.add("webview-active");
    ctx.elements.terminalEmpty.hidden = true;
    ctx.elements.newOutputButton.hidden = true;
    clearInterval(webview.timer);
    webview.timer = setInterval(() => { if (!document.hidden) refreshWebviewPorts(); }, WEBVIEW_REFRESH_MS);
    updateTabs();
  }

  function hideWebview() {
    if (!webviewActive) return;
    webviewActive = false;
    clearInterval(webview.timer);
    webview.hidden = true;
    ctx.elements.terminalPanel.classList.remove("webview-active");
    updateTabs();
  }

  function closeWebview() {
    hideWebview();
    webview.state = null;
    webview.frame.removeAttribute("src");
    webview.frame.src = "about:blank";
    webviewTab.remove();
    ctx.restore?.();
  }

  function updateTabs() {
    if (!ctx || !webviewTab) return;
    if (!webview.state) {
      webviewTab.remove();
      return;
    }
    webviewTab.setAttribute("aria-selected", String(webviewActive));
    ctx.elements.terminalTabs.append(webviewTab);
    if (webviewActive) {
      for (const tab of ctx.elements.terminalTabs.querySelectorAll("[data-terminal-id], #file-viewer-tab")) {
        tab.setAttribute("aria-selected", "false");
      }
    }
  }

  // ---------- git panel ----------

  function openGit(state) {
    const branch = el("span", "git-branch-name", "…");
    const { header, sub } = panelHeader([el("span", "git-branch-icon"), branch], state.workspace.path, () => loadGit(state));
    header.querySelector(".git-branch-icon").innerHTML = GIT_ICON;
    const files = el("ul", "git-file-list");
    files.setAttribute("aria-label", "Changed files");
    const diff = el("div", "git-diff");
    diff.append(el("p", "tools-empty", "Select a file to see its diff"));
    const body = el("div", "git-body");
    body.append(files, diff);
    state.node.append(header, body);
    Object.assign(state, { branch, sub, files, diff, selected: null, diffSeq: 0 });
    loadGit(state);
  }

  async function loadGit(state) {
    const id = ++state.seq;
    let data;
    try {
      ({ body: data } = await ctx.request(`/git/status?${query({ workspace_id: state.workspaceId })}`, { method: "GET" }));
    } catch (error) {
      if (panel !== state || id !== state.seq) return;
      state.branch.textContent = "Git unavailable";
      state.files.replaceChildren(el("li", "tools-empty is-error", error.message || "Git status failed"));
      return;
    }
    if (panel !== state || id !== state.seq) return;
    if (!data?.repository) {
      state.branch.textContent = "Not a git repository";
      state.files.replaceChildren(el("li", "tools-empty", "This workspace is not inside a git work tree."));
      state.diff.replaceChildren();
      return;
    }
    const b = data.branch || {};
    const head = b.head && b.head !== "(detached)" ? b.head : `detached @ ${String(b.oid || "").slice(0, 8)}`;
    state.branch.textContent = head;
    const parts = [];
    if (b.upstream) parts.push(b.upstream);
    if (b.ahead) parts.push(`↑${b.ahead}`);
    if (b.behind) parts.push(`↓${b.behind}`);
    parts.push(`${data.files.length}${data.truncated ? "+" : ""} changed file${data.files.length === 1 ? "" : "s"}`);
    state.sub.textContent = parts.join(" · ");
    state.sub.title = data.toplevel;
    if (!data.files.length) {
      state.files.replaceChildren(el("li", "tools-empty", "Working tree clean"));
      state.diff.replaceChildren();
      state.selected = null;
      return;
    }
    state.files.replaceChildren(...data.files.map((file) => fileRow(state, file)));
    const again = data.files.find((file) => file.path === state.selected);
    if (again) loadDiff(state, again, true);
    else loadDiff(state, data.files[0]);
  }

  function statusLetter(file) {
    if (file.status === "untracked") return "U";
    if (file.status === "conflict") return "!";
    return { added: "A", deleted: "D", renamed: "R", copied: "C", "type-changed": "T" }[file.status] || "M";
  }

  function fileRow(state, file) {
    const item = el("li", "git-file");
    const row = button("git-file-button", "", `${file.status} ${file.path}`, () => loadDiff(state, file));
    row.dataset.path = file.path;
    const slash = file.path.lastIndexOf("/");
    const letter = el("span", `git-status is-${file.status}`, statusLetter(file));
    const name = el("span", "git-file-name", file.path.slice(slash + 1));
    const dir = el("span", "git-file-dir", slash > 0 ? file.path.slice(0, slash) : "");
    const staged = file.index !== "." && file.index !== "?" ? el("span", "git-staged", "staged") : null;
    row.append(letter, name, dir);
    if (staged) row.append(staged);
    row.setAttribute("aria-current", String(file.path === state.selected));
    item.append(row);
    return item;
  }

  async function loadDiff(state, file, keepScroll = false) {
    state.selected = file.path;
    for (const row of state.files.querySelectorAll(".git-file-button")) {
      row.setAttribute("aria-current", String(row.dataset.path === file.path));
    }
    const id = ++state.diffSeq;
    const scroll = keepScroll ? state.diff.querySelector(".git-diff-lines")?.scrollTop || 0 : 0;
    if (!keepScroll) state.diff.replaceChildren(el("p", "tools-empty", "Loading diff…"));
    let data;
    try {
      ({ body: data } = await ctx.request(`/git/diff?${query({ workspace_id: state.workspaceId, path: file.path })}`, { method: "GET" }));
    } catch (error) {
      if (panel !== state || id !== state.diffSeq) return;
      state.diff.replaceChildren(el("p", "tools-empty is-error", error.message || "Diff unavailable"));
      return;
    }
    if (panel !== state || id !== state.diffSeq) return;
    const head = el("div", "git-diff-header");
    head.append(el("code", "git-diff-path", file.orig_path ? `${file.orig_path} → ${file.path}` : file.path));
    if (data.untracked) head.append(el("span", "tools-badge", "untracked"));
    if (data.truncated) head.append(el("span", "tools-badge is-warn", "truncated"));
    const lines = el("div", "git-diff-lines");
    const text = data.diff || "";
    if (!text.trim()) {
      lines.append(el("p", "tools-empty", file.status === "deleted" ? "File deleted" : "No textual changes"));
    } else {
      const all = text.split("\n");
      const frag = document.createDocumentFragment();
      for (const line of all.slice(0, DIFF_LINE_LIMIT)) frag.append(el("div", `git-line ${lineClass(line)}`, line || " "));
      if (all.length > DIFF_LINE_LIMIT) frag.append(el("div", "git-line is-meta", `… ${all.length - DIFF_LINE_LIMIT} more lines`));
      lines.append(frag);
    }
    state.diff.replaceChildren(head, lines);
    lines.scrollTop = scroll;
  }

  function lineClass(line) {
    if (/^(diff |index |--- |\+\+\+ |new file|deleted file|similarity|rename |old mode|new mode|Binary )/.test(line)) return "is-meta";
    if (line.startsWith("@@")) return "is-hunk";
    if (line.startsWith("+")) return "is-add";
    if (line.startsWith("-")) return "is-del";
    return "";
  }

  // ---------- context menu (rename / delete moved off the row) ----------

  function showMenu(event, items) {
    closeMenu();
    const visible = items.filter((item) => !item.hidden);
    if (!visible.length) return;
    event.preventDefault();
    menu = el("div", "tools-menu");
    menu.setAttribute("role", "menu");
    for (const item of visible) {
      const entry = button(`tools-menu-item${item.danger ? " is-danger" : ""}`, item.label, "", () => {
        closeMenu();
        item.run();
      });
      entry.setAttribute("role", "menuitem");
      menu.append(entry);
    }
    document.body.append(menu);
    const rect = menu.getBoundingClientRect();
    menu.style.left = `${Math.min(event.clientX, window.innerWidth - rect.width - 8)}px`;
    menu.style.top = `${Math.min(event.clientY, window.innerHeight - rect.height - 8)}px`;
    menu.querySelector("button")?.focus();
  }

  function closeMenu() {
    menu?.remove();
    menu = null;
  }

  function clear() {
    closePanel();
    closeMenu();
    if (webview?.state) {
      hideWebview();
      webview.state = null;
      webview.frame.src = "about:blank";
      webviewTab.remove();
    }
  }

  window.WebTermTools = {
    init, icon, toggle, closePanel, showMenu, openWebview,
    showWebview, hideWebview, isWebviewActive: () => webviewActive, updateTabs, clear,
  };
})();
