(() => {
  "use strict";
  // Click text in a terminal to preview the URL or file path under the pointer.
  // URLs open in a fully-permissioned iframe; paths resolve against the
  // terminal's cwd, then the workspace, then a workspace-wide file-name search.
  const URL_PATTERN = /\b(?:https?|ftp):\/\/[^\s"'`<>]+/g;
  const PATH_CHAR = /[^\s"'`<>()[\]{}|,;*]/;
  const BARE_NAMES = /^(?:Makefile|Dockerfile|Containerfile|Justfile|Procfile|Gemfile|Rakefile|Vagrantfile|LICENSE|README|CHANGELOG|AUTHORS|CODEOWNERS)$/;
  const IFRAME_ALLOW = [
    "camera", "microphone", "geolocation", "clipboard-read", "clipboard-write", "fullscreen",
    "display-capture", "autoplay", "encrypted-media", "picture-in-picture", "web-share", "midi",
    "usb", "serial", "hid", "bluetooth", "payment", "screen-wake-lock", "xr-spatial-tracking",
    "gyroscope", "accelerometer", "magnetometer", "idle-detection", "local-fonts", "storage-access",
  ].map((feature) => `${feature} *`).join("; ");
  const CLICK_SLOP_PX = 5;
  const EAGER_PREVIEWS = 4;

  let ctx, float, current = null, sequence = 0;

  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };

  function init(context) {
    ctx = context;
    float = el("aside", "terminal-link-preview");
    float.hidden = true;
    float.setAttribute("aria-label", "Terminal link preview");
    ctx.elements.terminalPanel.append(float);
  }

  function attach(session) {
    const surface = session.surface;
    let down = null;
    const pointerDown = (event) => {
      down = event.button === 0 ? { x: event.clientX, y: event.clientY } : null;
    };
    const click = (event) => {
      const start = down;
      down = null;
      if (!start || event.button !== 0 || event.detail > 1) return;
      if (event.shiftKey || event.altKey || event.ctrlKey || event.metaKey) return;
      if (Math.hypot(event.clientX - start.x, event.clientY - start.y) > CLICK_SLOP_PX) return;
      if (!event.target.closest?.(".xterm-screen")) return;
      const hit = hitTest(session, event);
      // Clicking text that is neither a URL nor a path dismisses any open preview.
      if (!hit) {
        hide();
        return;
      }
      // A second click on the same text closes the preview it opened.
      if (current && !float.hidden && current.sessionId === session.id && current.key === hit.key) {
        hide();
        return;
      }
      if (hit.url) showUrl(session, hit, event);
      else resolvePath(session, hit, event);
    };
    // Capture phase: xterm may consume events for application mouse reporting.
    surface.addEventListener("pointerdown", pointerDown, true);
    surface.addEventListener("click", click, true);
    return () => {
      surface.removeEventListener("pointerdown", pointerDown, true);
      surface.removeEventListener("click", click, true);
    };
  }

  function cellAt(session, event) {
    const screen = session.surface.querySelector(".xterm-screen");
    if (!screen) return null;
    const rect = screen.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    const { cols, rows } = session.term;
    const column = Math.floor((event.clientX - rect.left) / rect.width * cols);
    const row = Math.floor((event.clientY - rect.top) / rect.height * rows);
    if (column < 0 || column >= cols || row < 0 || row >= rows) return null;
    return { column, row: session.term.buffer.active.viewportY + row };
  }

  // Joins soft-wrapped rows so a long path or URL is read as one string.
  function logicalLine(buffer, row, column) {
    let start = row;
    while (start > 0 && buffer.getLine(start)?.isWrapped) start -= 1;
    let end = row;
    while (end - start < 64 && buffer.getLine(end + 1)?.isWrapped) end += 1;
    let text = "";
    let offset = -1;
    for (let index = start; index <= end; index += 1) {
      const line = buffer.getLine(index);
      if (!line) break;
      if (index === row) offset = text.length + line.translateToString(false, 0, column).length;
      text += line.translateToString(index === end);
    }
    return { text, offset, start };
  }

  function hitTest(session, event) {
    const cell = cellAt(session, event);
    if (!cell) return null;
    const { text, offset, start } = logicalLine(session.term.buffer.active, cell.row, cell.column);
    if (offset < 0 || offset >= text.length || /\s/.test(text[offset])) return null;

    for (const match of text.matchAll(URL_PATTERN)) {
      const url = trimUrl(match[0]);
      if (offset >= match.index && offset < match.index + url.length) {
        return { url, key: `${start}:${match.index}:${url}` };
      }
    }

    let from = offset;
    let to = offset;
    while (from > 0 && PATH_CHAR.test(text[from - 1])) from -= 1;
    while (to < text.length && PATH_CHAR.test(text[to])) to += 1;
    const path = cleanPath(text.slice(from, to));
    return path ? { path, key: `${start}:${from}:${path}` } : null;
  }

  function trimUrl(url) {
    let value = url.replace(/[.,;:!?'"]+$/, "");
    // Keep a closing bracket only when the URL itself opened one, e.g. wiki links.
    for (const [open, close] of [["(", ")"], ["[", "]"], ["{", "}"]]) {
      while (value.endsWith(close) && value.split(open).length < value.split(close).length) value = value.slice(0, -1);
    }
    return value.replace(/[.,;:!?'"]+$/, "");
  }

  function cleanPath(token) {
    let value = token
      .replace(/^[^\w~./@-]+/, "")
      .replace(/^@(?=[\w.~/])/, "")
      .replace(/^(?:a|b)\/(?=[\w.])/, (prefix) => prefix) // keep git-diff prefixes; server search handles them
      .replace(/#L\d+(?:-L?\d+)?$/, "")
      .replace(/(?::\d+){1,2}:?$/, "")
      .replace(/[.:]+$/, "");
    if (value.length < 2 || value.length > 1024) return "";
    if (/^[\d.:/-]+$/.test(value)) return "";
    const looksLikePath = value.includes("/") || /\.[A-Za-z0-9_-]{1,12}$/.test(value) || BARE_NAMES.test(value);
    if (!looksLikePath || value === "/" || value === "./" || value === "../") return "";
    return value;
  }

  // Server-local addresses are unreachable from a remote browser; route them through WebTerm's port proxy.
  function browserUrl(url) {
    let parsed;
    try { parsed = new URL(url); } catch (_) { return url; }
    const local = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "::1"].includes(parsed.hostname);
    const browserLocal = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
    if (!local || browserLocal || !/^https?:$/.test(parsed.protocol)) return url;
    const port = parsed.port || (parsed.protocol === "https:" ? "443" : "80");
    return `${location.origin}/proxy/${port}${parsed.pathname}${parsed.search}${parsed.hash}`;
  }

  function open(session, hit, event, title, subtitle) {
    sequence += 1;
    current = { sessionId: session.id, key: hit.key };
    float.querySelectorAll("audio,video").forEach((node) => node.pause());
    float.replaceChildren();
    const header = el("div", "link-preview-header");
    const heading = el("div", "link-preview-heading");
    const name = el("code", "link-preview-title", title);
    name.title = title;
    heading.append(name);
    if (subtitle) heading.append(el("span", "link-preview-subtitle", subtitle));
    const close = el("button", "icon-button link-preview-close", "×");
    close.type = "button";
    close.setAttribute("aria-label", "Close preview");
    close.addEventListener("click", hide);
    header.append(heading, close);
    const body = el("div", "link-preview-body");
    float.append(header, body);
    place(event);
    float.hidden = false;
    return { body, heading, id: sequence };
  }

  function place(event) {
    const panel = ctx.elements.terminalPanel.getBoundingClientRect();
    const width = Math.min(760, Math.max(280, panel.width - 24));
    const height = Math.min(Math.round(panel.height * 0.72), 640);
    const x = event.clientX - panel.left;
    const y = event.clientY - panel.top;
    const left = Math.max(12, Math.min(panel.width - width - 12, x - width / 2));
    // Open away from the clicked row so the clicked text stays visible.
    const below = y < panel.height / 2;
    float.style.width = `${width}px`;
    float.style.maxHeight = `${Math.max(180, below ? panel.height - y - 28 : y - 28)}px`;
    float.style.height = "";
    float.style.left = `${left}px`;
    float.style.top = below ? `${y + 18}px` : "";
    float.style.bottom = below ? "" : `${panel.height - y + 18}px`;
    float.dataset.height = String(height);
  }

  function showUrl(session, hit, event) {
    const target = browserUrl(hit.url);
    const { body } = open(session, hit, event, hit.url, target !== hit.url ? "via WebTerm port proxy" : "Web page");
    const bar = el("div", "link-preview-actions");
    const tab = el("a", "text-button", "Open in new tab ↗");
    tab.href = target;
    tab.target = "_blank";
    tab.rel = "noopener";
    bar.append(tab);
    if (target !== hit.url) {
      const original = el("a", "text-button", "Open original ↗");
      original.href = hit.url;
      original.target = "_blank";
      original.rel = "noopener";
      bar.append(original);
    }
    const frame = el("iframe", "link-preview-frame");
    frame.title = `Preview of ${hit.url}`;
    frame.src = target;
    frame.allow = IFRAME_ALLOW;
    frame.allowFullscreen = true;
    frame.style.height = `${Math.max(160, Number(float.dataset.height) - 90)}px`;
    body.append(bar, frame, el("p", "preview-safety", "Pages that forbid framing stay blank here — use Open in new tab."));
    float.classList.add("is-url");
  }

  async function resolvePath(session, hit, event) {
    const id = ++sequence;
    const query = new URLSearchParams({
      workspace_id: session.terminal.workspaceId,
      terminal_id: session.id,
      text: hit.path,
    });
    let result;
    try {
      ({ body: result } = await ctx.request(`/files/resolve?${query}`, { method: "GET" }));
    } catch (_) {
      result = null;
    }
    // Ignore stale lookups and never show an empty preview.
    if (id !== sequence) return;
    if (!result?.matches?.length) {
      hide();
      return;
    }
    const matches = result.matches;
    const summary = result.exact
      ? matches[0].is_dir ? "Folder" : "File"
      : `${matches.length}${result.truncated ? "+" : ""} match${matches.length === 1 ? "" : "es"} in workspace`;
    const { body } = open(session, hit, event, hit.path, summary);
    float.classList.remove("is-url");
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        observer.unobserve(entry.target);
        entry.target._load?.();
      }
    }, { root: body, rootMargin: "200px" });
    matches.forEach((match, index) => {
      const slot = el("div", "link-preview-item");
      slot.append(el("p", "preview-message", match.relative_path || match.path));
      slot._load = () => loadMatch(slot, match);
      body.append(slot);
      if (index < EAGER_PREVIEWS) slot._load();
      else observer.observe(slot);
    });
  }

  async function loadMatch(slot, match) {
    slot._load = null;
    const ws = { id: match.workspace_id, path: match.workspace_path };
    try {
      slot.replaceChildren(await window.WebTermExplorer.previewElement(ws, match));
    } catch (error) {
      slot.replaceChildren(el("p", "preview-message is-error", `${match.path}: ${error.message || "Preview unavailable"}`));
    }
  }

  function hide() {
    sequence += 1;
    current = null;
    if (!float) return;
    float.querySelectorAll("audio,video").forEach((node) => node.pause());
    float.hidden = true;
    float.classList.remove("is-url");
    float.replaceChildren();
  }

  window.WebTermLinks = { init, attach, hide, IFRAME_ALLOW, _test: { cleanPath, trimUrl, browserUrl } };
})();
