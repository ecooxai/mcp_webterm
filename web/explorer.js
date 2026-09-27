(() => {
  "use strict";
  const FOLDER =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><path d="M3 7V5a1 1 0 0 1 1-1h5l2 3h9a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V7Z"/></svg>';
  const CHEVRON =
    '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.5"><path d="m4 3 3 3-3 3"/></svg>';
  const DOCUMENT =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M6 3h8l4 4v14H6Z M14 3v5h4 M9 12h6m-6 4h6"/></svg>';
  const CONSOLE =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="3"/><path d="m7 9 3 3-3 3m6 0h4"/></svg>';
  const models = new Map();
  const history = new Map();
  let ctx,
    mini,
    viewer,
    viewerTab,
    selected = null,
    previewSequence = 0,
    viewerActive = false,
    initialized = false,
    busy = false;
  const el = (tag, className, text) => {
    const n = document.createElement(tag);
    if (className) n.className = className;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  const button = (label, className, run) => {
    const b = el("button", className, label);
    b.type = "button";
    if (run) b.addEventListener("click", run);
    return b;
  };
  const basename = (path) =>
    String(path).replace(/\/+$/, "").split("/").pop() || "/";
  const bytes = (size) => {
    if (size < 1024) return `${size} B`;
    const i = Math.min(3, Math.floor(Math.log(size) / Math.log(1024)));
    return `${(size / 1024 ** i).toFixed(i === 1 ? 0 : 1)} ${["B", "KB", "MB", "GB"][i]}`;
  };
  function model(ws) {
    let m = models.get(ws.id);
    if (!m) {
      m = {
        ws,
        mode: "files",
        open: new Set(),
        cache: new Map(),
        known: null,
        unread: false,
        pathShown: false,
        filter: "",
      };
      models.set(ws.id, m);
    }
    m.ws = ws;
    return m;
  }
  function rawURL(ws, entry) {
    const root = entry.workspace_path || ws.path;
    const relative =
      entry.relative_path ??
      entry.path.slice(root.replace(/\/$/, "").length + 1);
    return `/api/v1/files/raw/${encodeURIComponent(ws.id)}/${String(relative).split("/").map(encodeURIComponent).join("/")}`;
  }
  function icon(kind) {
    return (
      {
        folder: "▸",
        text: "≡",
        html: "◇",
        image: "▧",
        audio: "♪",
        video: "▷",
        model: "⬡",
        binary: "·",
      }[kind] || "·"
    );
  }
  function init(context) {
    ctx = context;
    mini = el("aside", "file-mini-preview");
    mini.id = "file-mini-preview";
    mini.hidden = true;
    mini.setAttribute("aria-label", "File quick preview");
    viewer = el("section", "file-viewer");
    viewer.id = "file-viewer";
    viewer.hidden = true;
    viewer.setAttribute("role", "tabpanel");
    viewer.setAttribute("aria-label", "File viewer history");
    viewer.setAttribute("aria-labelledby", "file-viewer-tab");
    const header = el("div", "viewer-toolbar");
    header.append(
      el("div", "", "File viewer"),
      el("span", "muted", "Newest first · previous previews stay below"),
    );
    const stack = el("div", "file-preview-stack");
    stack.id = "file-preview-stack";
    viewer.append(header, stack);
    viewerTab = button("", "terminal-tab file-viewer-tab", showViewer);
    viewerTab.id = "file-viewer-tab";
    viewerTab.dataset.key = "__file_viewer__";
    viewerTab.setAttribute("role", "tab");
    viewerTab.setAttribute("aria-controls", "file-viewer");
    viewerTab.append(
      el("span", "", "▧"),
      el("span", "file-viewer-tab-label", "Viewer"),
    );
    ctx.elements.terminalPanel.append(viewer, mini);
    document.addEventListener("pointerdown", (event) => {
      if (mini.hidden) return;
      const target = event.target;
      if (mini.contains(target) || target.closest?.(".file-tree-row")) return;
      closeMini();
    });
    setInterval(() => refresh(), 5000);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) refresh();
    });
  }
  function observe(workspaces) {
    for (const ws of workspaces) {
      const m = model(ws);
      const ids = new Set(ws.terminals.map((t) => t.id));
      if (m.known && [...ids].some((id) => !m.known.has(id))) m.unread = true;
      m.known = ids;
    }
    for (const id of models.keys())
      if (!workspaces.some((ws) => ws.id === id)) models.delete(id);
    if (!initialized && workspaces.length) {
      ctx.state.expanded.add(workspaces[0].id);
      initialized = true;
    }
  }
  function pane() {
    const content = el("div", "workspace-content"),
      area = el("div", "workspace-files"),
      toolbar = el("div", "file-tree-toolbar"),
      title = el("span", "file-tree-title", "Files");
    const refresh = button("", "file-tree-refresh");
    refresh.innerHTML =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M20 7v5h-5M20 12a8 8 0 1 0-2 5"/></svg>';
    refresh.title = "Refresh files now";
    refresh.setAttribute("aria-label", "Refresh workspace files");
    const info = el("span", "file-tree-info", "Every 5s");
    toolbar.append(title, info, refresh);
    const search = el("input", "file-tree-search");
    search.type = "search";
    search.placeholder = "Filter files…";
    search.setAttribute("aria-label", "Filter loaded workspace files");
    search.title =
      "Filter names in loaded folders; expand folders to include their contents";
    const tree = el("div", "folder-tree");
    tree.setAttribute("role", "tree");
    tree.setAttribute("aria-label", "Workspace files");
    area.append(toolbar, search, tree);
    const terminals = el("div", "terminal-list");
    terminals.setAttribute("role", "group");
    content.append(area, terminals);
    return content;
  }
  function updateWorkspace(node, ws) {
    const m = model(ws);
    m.node = node;
    const open = ctx.state.expanded.has(ws.id);
    node.querySelector(".workspace-content").hidden = !open;
    node.querySelector(".workspace-files").hidden = m.mode !== "files";
    node.querySelector(".terminal-list").hidden = m.mode !== "terminals";
    node.querySelector(".workspace-new-dot").hidden = !m.unread;
    const toggle = node.querySelector('[data-action="workspace-mode"]');
    toggle.innerHTML = m.mode === "files" ? FOLDER : CONSOLE;
    toggle.setAttribute(
      "aria-label",
      m.mode === "files"
        ? `Show terminals in ${basename(ws.path)}`
        : `Show files in ${basename(ws.path)}`,
    );
    toggle.title =
      m.mode === "files"
        ? "Files · click for terminals"
        : "Terminals · click for files";
    toggle.setAttribute("aria-pressed", String(m.mode === "files"));
    node.querySelector(".workspace-full-path").hidden = true;
    const search = node.querySelector(".file-tree-search");
    if (!search.dataset.bound) {
      search.dataset.bound = "true";
      search.addEventListener("input", () => {
        model(ws).filter = search.value.toLowerCase();
        renderTree(model(ws));
      });
      node
        .querySelector(".file-tree-refresh")
        .addEventListener("click", () => refreshWorkspace(model(ws)));
    }
    if (open && m.mode === "files") {
      renderTree(m);
      if (!m.cache.has(ws.path)) loadFolder(m, ws.path);
    }
    if (open && m.mode === "terminals" && m.unread) {
      m.unread = false;
      node.querySelector(".workspace-new-dot").hidden = true;
    }
  }
  function toggleMode(id) {
    const m = models.get(id);
    if (!m) return;
    m.mode = m.mode === "files" ? "terminals" : "files";
    ctx.state.expanded.add(id);
    if (m.mode === "terminals") m.unread = false;
    ctx.renderNavigation();
  }
  function togglePath(id) {
    const m = models.get(id);
    if (m) {
      m.pathShown = !m.pathShown;
      ctx.renderNavigation();
    }
  }
  async function copyPath(id) {
    const m = models.get(id);
    if (m) await copy(m.ws.path);
  }
  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      ctx.toast("Path copied");
    } catch {
      ctx.toast("Clipboard unavailable. Select the path to copy it.");
    }
  }
  function reconcile(parent, items, key, create, update) {
    const existing = new Map(
      [...parent.children]
        .filter((n) => n.dataset.key)
        .map((n) => [n.dataset.key, n]),
    );
    const keep = new Set();
    let cursor = parent.firstElementChild;
    for (const item of items) {
      const k = String(key(item));
      let n = existing.get(k);
      if (!n) {
        n = create(item);
        n.dataset.key = k;
      }
      update(n, item);
      keep.add(k);
      if (n !== cursor) parent.insertBefore(n, cursor);
      cursor = n.nextElementSibling;
    }
    for (const [k, n] of existing) if (!keep.has(k)) n.remove();
  }
  function visibleDirs(m) {
    const found = [m.ws.path];
    function visit(path, depth) {
      if (depth > 32) return;
      for (const entry of m.cache.get(path)?.entries || []) {
        if (entry.is_dir && m.open.has(entry.path)) {
          found.push(entry.path);
          visit(entry.path, depth + 1);
        }
      }
    }
    visit(m.ws.path, 0);
    return found;
  }
  async function loadFolder(m, path, more = false) {
    let cache = m.cache.get(path);
    if (cache?.loading) return;
    if (!cache) {
      cache = {
        entries: [],
        newRanks: new Map(),
        initialized: false,
        loading: false,
        next: null,
        error: "",
      };
      m.cache.set(path, cache);
    }
    cache.loading = true;
    if (!cache.initialized) renderTree(m);
    try {
      const entries = [];
      let offset = more ? cache.next || 0 : 0;
      let next = null;
      const target = more
        ? 1
        : Math.max(1, Math.ceil(cache.entries.length / 500));
      for (let i = 0; i < target; i++) {
        const { body } = await ctx.request(
          `/files/list?${new URLSearchParams({ workspace_id: m.ws.id, path, offset: String(offset) })}`,
          { method: "GET" },
        );
        entries.push(...body.entries);
        next = body.next_offset;
        cache.total = body.total;
        if (next === null) break;
        offset = next;
      }
      const old = new Set(cache.entries.map((e) => e.path));
      if (cache.initialized && !more) {
        for (const entry of entries)
          if (!old.has(entry.path) && !cache.newRanks.has(entry.path))
            cache.newRanks.set(entry.path, Date.now());
      }
      cache.entries = more
        ? [...cache.entries, ...entries.filter((e) => !old.has(e.path))]
        : entries;
      cache.next = next;
      cache.error = "";
      cache.initialized = true;
      cache.last = Date.now();
    } catch (error) {
      cache.error = error.message || "Folder unavailable";
    } finally {
      cache.loading = false;
      if (models.has(m.ws.id)) renderTree(m);
    }
  }
  async function refreshWorkspace(m) {
    if (!ctx.state.expanded.has(m.ws.id) || m.mode !== "files") return;
    const paths = visibleDirs(m);
    for (let i = 0; i < paths.length; i += 4)
      await Promise.all(
        paths.slice(i, i + 4).map((path) => loadFolder(m, path)),
      );
  }
  async function refresh() {
    if (!ctx?.state.authenticated || document.hidden || busy) return;
    busy = true;
    try {
      const active = [...models.values()].filter(
        (m) => ctx.state.expanded.has(m.ws.id) && m.mode === "files",
      );
      for (let i = 0; i < active.length; i += 3)
        await Promise.all(
          active.slice(i, i + 3).map((m) => refreshWorkspace(m)),
        );
    } finally {
      busy = false;
    }
  }
  function renderTree(m) {
    if (!m.node) return;
    const parent = m.node.querySelector(".folder-tree"),
      scroll = parent.scrollTop;
    renderFolder(m, m.ws.path, parent, 0);
    parent.scrollTop = scroll;
    const root = m.cache.get(m.ws.path),
      info = m.node.querySelector(".file-tree-info");
    info.textContent = root?.error
      ? "Retrying…"
      : root?.loading
        ? "Refreshing…"
        : "Every 5s";
    info.title =
      root?.error || "Expanded folders refresh automatically every 5 seconds";
  }
  function renderFolder(m, path, parent, depth) {
    const cache = m.cache.get(path);
    if (!cache) {
      if (!parent.children.length)
        parent.append(el("p", "tree-message", "Loading…"));
      return;
    }
    for (const msg of parent.querySelectorAll(
      ":scope > .tree-message, :scope > .tree-load-more",
    ))
      msg.remove();
    let entries = [...cache.entries];
    entries.sort(
      (a, b) =>
        (cache.newRanks.get(b.path) || 0) - (cache.newRanks.get(a.path) || 0) ||
        Number(b.is_dir) - Number(a.is_dir) ||
        a.name.localeCompare(b.name, undefined, {
          numeric: true,
          sensitivity: "base",
        }),
    );
    if (m.filter)
      entries = entries.filter(
        (e) => e.is_dir || e.name.toLowerCase().includes(m.filter),
      );
    reconcile(
      parent,
      entries,
      (e) => e.path,
      (e) => {
        const item = el("div", "file-tree-item"),
          row = button("", "file-tree-row");
        row.setAttribute("role", "treeitem");
        row.append(
          el("span", "file-tree-chevron"),
          el("span", "file-kind"),
          el("span", "file-tree-name"),
          el("span", "file-new-badge", "New"),
          el("span", "file-tree-size"),
        );
        row.addEventListener("click", () => {
          const entry = row._entry;
          if (entry.is_dir) {
            if (m.open.has(entry.path)) m.open.delete(entry.path);
            else {
              m.open.add(entry.path);
              loadFolder(m, entry.path);
            }
            renderTree(m);
          } else selectFile(m.ws, entry);
        });
        row.addEventListener("keydown", (event) => {
          const entry = row._entry;
          if (
            event.key === "ArrowRight" &&
            entry.is_dir &&
            !m.open.has(entry.path)
          ) {
            event.preventDefault();
            row.click();
          } else if (
            event.key === "ArrowLeft" &&
            entry.is_dir &&
            m.open.has(entry.path)
          ) {
            event.preventDefault();
            row.click();
          } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            const rows = [...m.node.querySelectorAll(".file-tree-row")].filter(
              (n) => n.getClientRects().length,
            );
            rows[
              rows.indexOf(row) + (event.key === "ArrowDown" ? 1 : -1)
            ]?.focus();
          } else if (
            event.key === "Enter" &&
            (event.ctrlKey || event.metaKey) &&
            !entry.is_dir
          ) {
            event.preventDefault();
            openLarge(m.ws, entry);
          }
        });
        const children = el("div", "file-tree-children");
        children.setAttribute("role", "group");
        item.append(row, children);
        return item;
      },
      (item, e) => {
        const row = item.firstElementChild;
        row._entry = e;
        row.title = e.path;
        row.dataset.path = e.path;
        row.setAttribute(
          "aria-label",
          `${e.name}${e.is_dir ? ", folder" : ""}`,
        );
        row.setAttribute("aria-level", String(depth + 1));
        row.style.paddingLeft = `${8 + depth * 14}px`;
        row.querySelector(".file-tree-name").textContent = e.name;
        row.querySelector(".file-kind").innerHTML = e.is_dir
          ? FOLDER
          : DOCUMENT;
        row.querySelector(".file-kind").dataset.kind = e.kind;
        row.querySelector(".file-tree-chevron").innerHTML = e.is_dir
          ? CHEVRON
          : "";
        row.querySelector(".file-tree-chevron").style.transform = m.open.has(
          e.path,
        )
          ? "rotate(90deg)"
          : "";
        row.querySelector(".file-tree-size").textContent = e.is_dir
          ? ""
          : bytes(e.size);
        row.querySelector(".file-new-badge").hidden = !cache.newRanks.has(
          e.path,
        );
        row.classList.toggle(
          "is-selected",
          selected?.key === `${m.ws.id}:${e.path}`,
        );
        row.setAttribute(
          "aria-selected",
          String(selected?.key === `${m.ws.id}:${e.path}`),
        );
        const expanded = e.is_dir && m.open.has(e.path) && depth < 32;
        item.lastElementChild.hidden = !expanded;
        if (e.is_dir) row.setAttribute("aria-expanded", String(expanded));
        else row.removeAttribute("aria-expanded");
        if (expanded) renderFolder(m, e.path, item.lastElementChild, depth + 1);
      },
    );
    if (cache.error) {
      const error = el("p", "tree-message is-error", cache.error);
      error.append(button("Retry", "text-button", () => loadFolder(m, path)));
      parent.append(error);
    } else if (!entries.length)
      parent.append(
        el(
          "p",
          "tree-message",
          cache.loading
            ? "Loading…"
            : m.filter
              ? "No matching files"
              : "This folder is empty",
        ),
      );
    if (cache.next !== null) {
      const more = button(
        `Load more (${cache.entries.length} of ${cache.total})`,
        "tree-load-more",
        () => loadFolder(m, path, true),
      );
      more.disabled = cache.loading;
      parent.append(more);
    }
  }
  async function metadata(ws, entry) {
    const { body } = await ctx.request(
      `/files/preview?${new URLSearchParams({ workspace_id: ws.id, path: entry.path })}`,
      { method: "GET" },
    );
    return body;
  }
  async function selectFile(ws, entry) {
    const key = `${ws.id}:${entry.path}`;
    if (selected?.key === key) {
      openLarge(ws, entry);
      return;
    }
    selected = { key, ws, entry };
    const sequence = ++previewSequence;
    mini.hidden = false;
    mini.dataset.kind = entry.kind || "";
    mini.replaceChildren();
    const heading = el("div", "mini-toolbar");
    heading.append(
      el("strong", "", "Quick preview"),
      button("Open in viewer ↗", "text-button", () => openLarge(ws, entry)),
      button("×", "icon-button", closeMini),
    );
    heading.lastElementChild.setAttribute("aria-label", "Close file preview");
    const content = el("div", "mini-content");
    content.append(el("p", "preview-message", "Loading preview…"));
    mini.append(heading, content);
    ctx.layout();
    const m = models.get(ws.id);
    if (m) renderTree(m);
    try {
      const info = await metadata(ws, entry);
      if (sequence !== previewSequence) return;
      selected.info = info;
      mini.dataset.kind = info.kind;
      content.replaceChildren(previewCard(ws, info, true));
    } catch (error) {
      if (sequence === previewSequence)
        content.replaceChildren(
          el(
            "p",
            "preview-message is-error",
            error.message || "Preview unavailable",
          ),
        );
    }
  }
  function closeMini() {
    previewSequence++;
    mini.hidden = true;
    mini.querySelectorAll("audio,video").forEach((n) => n.pause());
    mini.replaceChildren();
    selected = null;
    ctx.layout();
    for (const m of models.values()) if (m.node) renderTree(m);
  }
  async function openLarge(ws, entry) {
    const key = `${ws.id}:${entry.path}`;
    showViewer();
    const stack = viewer.querySelector(".file-preview-stack");
    if (history.has(key)) {
      stack.prepend(history.get(key));
      viewer.scrollTop = 0;
      return;
    }
    const card = el("article", "file-preview-card");
    card.dataset.path = entry.path;
    card.append(el("p", "preview-message", "Loading preview…"));
    history.set(key, card);
    stack.prepend(card);
    viewer.scrollTop = 0;
    updateTabs();
    try {
      const info =
        selected?.key === key && selected.info
          ? selected.info
          : await metadata(ws, entry);
      const ready = previewCard(ws, info, false);
      card.replaceChildren(...ready.childNodes);
      card.dataset.kind = info.kind;
    } catch (error) {
      card.replaceChildren(
        el("header", "preview-path", entry.path),
        el(
          "p",
          "preview-message is-error",
          error.message || "Preview unavailable",
        ),
      );
    }
  }
  function previewCard(ws, info, small) {
    const card = el("article", "file-preview-card");
    card.dataset.path = info.path;
    card.dataset.kind = info.kind;
    const header = el("header", "preview-card-header"),
      path = el("code", "preview-path", info.path);
    path.title = info.path;
    const detail = el("div", "preview-card-meta");
    detail.append(
      el("span", "preview-type", info.kind.toUpperCase()),
      el("span", "", bytes(info.size)),
      button("Copy path", "text-button", () => copy(info.path)),
    );
    const link = el("a", "text-button", "Download");
    link.href = rawURL(ws, info);
    link.download = info.name;
    detail.append(link);
    header.append(path, detail);
    const body = el("div", "preview-card-body"),
      url = rawURL(ws, info);
    if (info.kind === "text") {
      const pre = el("pre", "file-text-preview");
      pre.append(el("code", "", info.text || ""));
      body.append(pre);
      if (info.truncated)
        body.append(
          el(
            "p",
            "preview-message",
            `Preview limited to ${bytes(info.text_limit)}. Download to read the full file.`,
          ),
        );
    } else if (info.kind === "image") {
      const img = el("img", "file-image-preview");
      img.alt = info.name;
      img.loading = "lazy";
      img.src = url;
      img.addEventListener("error", () =>
        body.append(
          el(
            "p",
            "preview-message is-error",
            "Image could not be decoded. Download is available.",
          ),
        ),
      );
      const facts = mediaFacts(info);
      img.addEventListener("load", () =>
        facts.set("Resolution", `${img.naturalWidth} × ${img.naturalHeight}`),
      );
      body.append(img, facts.node);
    } else if (info.kind === "audio" || info.kind === "video") {
      const media = el(info.kind, "file-media-preview");
      media.controls = true;
      media.preload = "metadata";
      media.src = url;
      if (info.kind === "video") media.playsInline = true;
      media.addEventListener("error", () =>
        body.append(
          el(
            "p",
            "preview-message is-error",
            "This browser could not decode the media. Download the original file.",
          ),
        ),
      );
      const facts = mediaFacts(info);
      media.addEventListener("loadedmetadata", () => {
        if (media.videoWidth)
          facts.set("Resolution", `${media.videoWidth} × ${media.videoHeight}`);
        if (Number.isFinite(media.duration) && media.duration > 0) {
          facts.set("Duration", duration(media.duration));
          if (!info.media?.bit_rate)
            facts.set("Bitrate", `≈ ${bitrate((info.size * 8) / media.duration)}`);
        }
      });
      body.append(media, facts.node);
    } else if (info.kind === "html") {
      const iframe = el("iframe", "file-html-preview");
      iframe.title = `HTML preview: ${info.name}`;
      iframe.setAttribute("sandbox", "allow-same-origin");
      iframe.referrerPolicy = "no-referrer";
      iframe.loading = "lazy";
      iframe.src = url;
      body.append(
        iframe,
        el("p", "preview-safety", "HTML preview · scripts and forms disabled"),
      );
    } else if (info.kind === "model") {
      const model = el("model-viewer", "file-model-preview");
      model.setAttribute("src", url);
      model.setAttribute("alt", info.name);
      model.setAttribute("camera-controls", "");
      model.setAttribute("touch-action", "pan-y");
      model.setAttribute("shadow-intensity", "1");
      model.setAttribute("interaction-prompt", "auto");
      model.setAttribute("loading", "lazy");
      const status = el("p", "preview-message", "Loading interactive 3D view…");
      model.addEventListener("load", () => {
        status.textContent =
          "Drag to orbit · scroll to zoom · right-drag to pan";
        model.dataset.loaded = "true";
      });
      model.addEventListener("error", () => {
        status.textContent =
          "3D preview unavailable. Check the model format or download the original.";
        status.classList.add("is-error");
      });
      body.append(model, status);
      loadModelViewer().catch(() => {
        status.textContent = "3D viewer could not be loaded. Reload to retry.";
      });
    } else
      body.append(
        el(
          "p",
          "preview-message",
          "No inline preview for this binary file. Download the original to open it.",
        ),
      );
    card.append(header, body);
    if (!small) {
      const collapse = button("Collapse", "text-button", () => {
        body.hidden = !body.hidden;
        collapse.textContent = body.hidden ? "Expand" : "Collapse";
      });
      detail.append(collapse);
    }
    return card;
  }
  function bitrate(bps) {
    return bps >= 1e6 ? `${(bps / 1e6).toFixed(2)} Mbps` : `${Math.round(bps / 1e3)} kbps`;
  }
  function duration(seconds) {
    const s = Math.round(seconds);
    const h = Math.floor(s / 3600),
      m = Math.floor((s % 3600) / 60),
      r = String(s % 60).padStart(2, "0");
    return h ? `${h}:${String(m).padStart(2, "0")}:${r}` : `${m}:${r}`;
  }
  function frameRate(value) {
    const [n, d] = String(value || "").split("/").map(Number);
    return n && d ? `${Math.round((n / d) * 100) / 100} fps` : "";
  }
  // Resolution, codec and bitrate facts; server ffprobe data first, browser values fill gaps.
  function mediaFacts(info) {
    const node = el("dl", "preview-media-facts"),
      rows = new Map();
    const set = (label, value) => {
      if (!value) return;
      let dd = rows.get(label);
      if (!dd) {
        dd = el("dd", "");
        rows.set(label, dd);
        node.append(el("dt", "", label), dd);
      }
      dd.textContent = value;
    };
    const m = info.media || {};
    if (m.width && m.height) set("Resolution", `${m.width} × ${m.height}`);
    set("Video codec", m.video_codec);
    set("Audio codec", m.audio_codec);
    set("Frame rate", frameRate(m.frame_rate));
    if (m.duration_s) set("Duration", duration(m.duration_s));
    if (m.bit_rate) set("Bitrate", bitrate(m.bit_rate));
    if (m.video_bit_rate) set("Video bitrate", bitrate(m.video_bit_rate));
    set("Type", info.mime_type);
    return { node, set };
  }
  let modelPromise;
  function loadModelViewer() {
    if (!modelPromise) modelPromise = import("/assets/model-viewer.min.js");
    return modelPromise;
  }
  function showViewer() {
    if (!ctx) return;
    viewerActive = true;
    viewer.hidden = false;
    ctx.elements.terminalPanel.classList.add("file-viewer-active");
    ctx.elements.terminalEmpty.hidden = true;
    ctx.elements.newOutputButton.hidden = true;
    updateTabs();
    viewer.scrollTop = 0;
    ctx.layout();
  }
  function hideViewer() {
    viewerActive = false;
    if (viewer) viewer.hidden = true;
    ctx?.elements.terminalPanel.classList.remove("file-viewer-active");
    viewer?.querySelectorAll("audio,video").forEach((n) => n.pause());
    updateTabs();
  }
  function updateTabs() {
    if (!ctx || !viewerTab) return;
    if (history.size || viewerActive) {
      viewerTab.querySelector(".file-viewer-tab-label").textContent =
        `Viewer${history.size ? ` · ${history.size}` : ""}`;
      viewerTab.setAttribute("aria-selected", String(viewerActive));
      viewerTab.tabIndex = 0;
      ctx.elements.terminalTabs.append(viewerTab);
    }
    if (viewerActive)
      for (const tab of ctx.elements.terminalTabs.querySelectorAll(
        "[data-terminal-id]",
      ))
        tab.setAttribute("aria-selected", "false");
  }
  function clear() {
    closeMini();
    hideViewer();
    history.clear();
    models.clear();
    initialized = false;
    viewer.querySelector(".file-preview-stack").replaceChildren();
    viewerTab.remove();
  }
  window.WebTermExplorer = {
    init,
    observe,
    pane,
    updateWorkspace,
    toggleMode,
    togglePath,
    copyPath,
    refresh,
    hideViewer,
    updateTabs,
    clear,
    isViewerActive: () => viewerActive,
    basename,
  };
})();
