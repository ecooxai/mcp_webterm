(() => {
  "use strict";
  let ctx,
    dialog,
    rows,
    info,
    filters,
    pager,
    timer = 0,
    loading = false,
    offset = 0,
    snapshot = null,
    generation = 0,
    paused = false;
  const expanded = new Map();
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  const btn = (text, cls, fn) => {
    const b = el("button", cls, text);
    b.type = "button";
    b.addEventListener("click", fn);
    return b;
  };
  const size = (n) =>
    n > 1048576
      ? `${(n / 1048576).toFixed(1)} MB`
      : n > 1024
        ? `${(n / 1024).toFixed(1)} KB`
        : `${n} B`;
  function init(context) {
    ctx = context;
    dialog = el("dialog", "tool-log-dialog");
    dialog.id = "tool-log-dialog";
    dialog.setAttribute("aria-labelledby", "tool-log-title");
    const head = el("header", "tool-log-header"),
      titles = el("div", "");
    titles.append(el("p", "eyebrow", "Observability"));
    const title = el("h2", "", "MCP tool calls");
    title.id = "tool-log-title";
    titles.append(
      title,
      el(
        "p",
        "tool-log-subtitle",
        "Private execution history · inputs, outputs, timing, and errors",
      ),
    );
    const actions = el("div", "tool-log-actions");
    const pause = btn("Pause live", "secondary-button", () => {
      paused = !paused;
      pause.textContent = paused ? "Resume live" : "Pause live";
      if (!paused) {
        offset = 0;
        snapshot = null;
        load();
      }
    });
    pause.id = "tool-log-pause";
    const link = el("a", "text-button", "Open /log ↗");
    link.href = location.pathname.startsWith("/webterm")
      ? "/webterm/log"
      : "/log";
    link.target = "_blank";
    link.rel = "noopener";
    actions.append(
      link,
      pause,
      btn("Sign out", "text-button", () => {
        close();
        ctx.logout();
      }),
    );
    const closeBtn = btn("×", "icon-button", close);
    closeBtn.setAttribute("aria-label", "Close tool logs");
    actions.append(closeBtn);
    head.append(titles, actions);
    filters = el("form", "tool-log-filters");
    filters.addEventListener("submit", (e) => {
      e.preventDefault();
      reset();
    });
    const search = el("input", "");
    search.type = "search";
    search.name = "q";
    search.placeholder = "Search tools, tasks, summaries…";
    search.setAttribute("aria-label", "Search tool logs");
    const workspace = el("input", "");
    workspace.name = "workspace";
    workspace.placeholder = "Workspace path";
    workspace.setAttribute("aria-label", "Filter logs by workspace path");
    const task = el("input", "");
    task.name = "task";
    task.placeholder = "Task name";
    task.setAttribute("aria-label", "Filter logs by task");
    const status = el("select", "");
    status.name = "status";
    status.setAttribute("aria-label", "Filter logs by status");
    for (const [value, text] of [
      ["", "All statuses"],
      ["running", "Running"],
      ["success", "Success"],
      ["error", "Error"],
      ["interrupted", "Interrupted"],
    ]) {
      const o = el("option", "", text);
      o.value = value;
      status.append(o);
    }
    const sort = el("select", "");
    sort.name = "sort";
    sort.setAttribute("aria-label", "Sort tool logs");
    for (const [value, text] of [
      ["date", "Newest first"],
      ["oldest", "Oldest first"],
      ["duration", "Longest duration"],
      ["input_size", "Largest input"],
      ["output_size", "Largest output"],
    ]) {
      const o = el("option", "", text);
      o.value = value;
      sort.append(o);
    }
    filters.append(
      search,
      workspace,
      task,
      status,
      sort,
      btn("Apply", "secondary-button", reset),
    );
    for (const field of [status, sort]) field.addEventListener("change", reset);
    let debounce;
    search.addEventListener("input", () => {
      clearTimeout(debounce);
      debounce = setTimeout(reset, 300);
    });
    info = el("div", "tool-log-info", "Loading…");
    info.setAttribute("role", "status");
    rows = el("div", "tool-log-rows");
    rows.id = "tool-log-rows";
    pager = el("footer", "tool-log-pager");
    pager.append(
      btn("← Previous", "secondary-button", () => {
        offset = Math.max(0, offset - 100);
        load();
        rows.scrollTop = 0;
      }),
      el("span", "tool-log-page"),
      btn("Next →", "secondary-button", () => {
        offset += 100;
        load();
        rows.scrollTop = 0;
      }),
    );
    const note = el(
      "span",
      "tool-log-retention",
      "Keeps 2,000 calls. Large payloads are bounded; credentials and images are redacted.",
    );
    pager.append(note);
    dialog.append(head, filters, info, rows, pager);
    dialog.addEventListener("cancel", (e) => {
      e.preventDefault();
      close();
    });
    document.body.append(dialog);
  }
  function reset() {
    offset = 0;
    snapshot = null;
    generation++;
    load();
  }
  function open() {
    if (!dialog) return;
    if (!dialog.open) dialog.showModal();
    reset();
    clearInterval(timer);
    timer = setInterval(() => {
      if (!paused && !document.hidden && offset === 0) {
        snapshot = null;
        load();
      }
    }, 5000);
  }
  function close() {
    clearInterval(timer);
    timer = 0;
    generation++;
    dialog?.close();
  }
  async function load() {
    if (!dialog.open) return;
    const seq = ++generation;
    const params = new URLSearchParams(new FormData(filters));
    params.set("offset", String(offset));
    if (snapshot !== null) params.set("snapshot", String(snapshot));
    try {
      const { body } = await ctx.request(`/tool-logs?${params}`, {
        method: "GET",
      });
      if (seq !== generation || !dialog.open) return;
      snapshot = body.snapshot;
      info.textContent = `${body.total} matching calls · ${paused ? "Live paused" : offset ? "History page" : "Updates every 5 seconds"} · ${new Date().toLocaleTimeString()}`;
      render(body.entries);
      pager.firstElementChild.disabled = offset === 0;
      pager.children[2].disabled = offset + 100 >= body.total;
      pager.querySelector(".tool-log-page").textContent = body.total
        ? `${offset + 1}–${Math.min(offset + 100, body.total)} of ${body.total}`
        : "0 calls";
    } catch (error) {
      if (seq === generation)
        info.textContent =
          error.message || "Logs could not be loaded. Retrying…";
    }
  }
  function render(entries) {
    const old = new Map(
      [...rows.children]
        .filter((n) => n.dataset.id)
        .map((n) => [n.dataset.id, n]),
    );
    const keep = new Set();
    const scroll = rows.scrollTop;
    for (const empty of rows.querySelectorAll(".tool-log-empty"))
      empty.remove();
    for (const entry of entries) {
      const key = String(entry.id);
      keep.add(key);
      let card = old.get(key);
      if (!card) {
        card = el("article", "tool-log-card");
        card.dataset.id = key;
        const summary = btn("", "tool-log-row", () => toggle(card, entry.id));
        summary.setAttribute("aria-expanded", "false");
        summary.append(
          el("span", "log-tool"),
          el("span", "log-status"),
          el("time", "log-time"),
          el("span", "log-duration"),
        );
        const context = el("div", "log-context");
        context.append(
          el("strong", "log-task"),
          el("span", "log-summary"),
          el("code", "log-workspace"),
          el("span", "log-sizes"),
        );
        const details = el("div", "tool-log-details");
        details.hidden = true;
        card.append(summary, context, details);
      }
      card.querySelector(".log-tool").textContent = entry.tool;
      const status = card.querySelector(".log-status");
      status.textContent = entry.status;
      status.dataset.status = entry.status;
      card.querySelector(".log-time").textContent = new Date(
        entry.started_ms,
      ).toLocaleString();
      card.querySelector(".log-duration").textContent =
        entry.duration_ms === null
          ? "In progress"
          : `${entry.duration_ms.toLocaleString()} ms`;
      card.querySelector(".log-task").textContent =
        entry.task || "Untracked call";
      card.querySelector(".log-summary").textContent = entry.summary;
      card.querySelector(".log-workspace").textContent = entry.workspace;
      card.querySelector(".log-sizes").textContent =
        `Input ${size(entry.input_size)} · Output ${size(entry.output_size)} · #${entry.id}`;
      const changed = card.dataset.status !== entry.status;
      card.dataset.status = entry.status;
      if (changed && !card.querySelector(".tool-log-details").hidden)
        detail(card, entry.id);
      rows.append(card);
    }
    for (const [key, node] of old) if (!keep.has(key)) node.remove();
    if (!entries.length)
      rows.append(
        el(
          "p",
          "tool-log-empty",
          "No matching tool calls yet. New MCP invocations will appear here.",
        ),
      );
    rows.scrollTop = scroll;
  }
  function toggle(card, id) {
    const panel = card.querySelector(".tool-log-details");
    panel.hidden = !panel.hidden;
    card
      .querySelector(".tool-log-row")
      .setAttribute("aria-expanded", String(!panel.hidden));
    if (!panel.hidden) detail(card, id);
  }
  async function detail(card, id) {
    const panel = card.querySelector(".tool-log-details");
    if (!panel.children.length) panel.textContent = "Loading payloads…";
    try {
      const { body } = await ctx.request(`/tool-logs/${id}`, { method: "GET" });
      panel.replaceChildren();
      for (const key of ["arguments", "output"]) {
        const group = el("section", "");
        group.append(el("h3", "", key === "arguments" ? "Input" : "Output"));
        const pre = el("pre", "");
        pre.textContent =
          body[key] === null
            ? "Still running…"
            : JSON.stringify(body[key], null, 2);
        group.append(pre);
        panel.append(group);
      }
    } catch (error) {
      panel.textContent = error.message || "Payload unavailable";
    }
  }
  window.WebTermLog = { init, open, close };
})();
