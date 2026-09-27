(() => {
  "use strict";
  const INTERVAL = 5000;
  const INTERACTION_DELAY = 10000;
  const state = { open: false, data: null, selected: null, sort: "cpu", direction: -1,
    pauseUntil: 0, timer: 0, ticker: 0, busy: false, request: null, controller: null,
    detailController: null, generation: 0, search: "", trigger: null, action: null, actionBusy: false };
  let root, el = {};
  const node = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text !== undefined) n.textContent = text; return n; };
  const valid = n => typeof n === "number" && Number.isFinite(n);
  const percent = n => valid(n) ? `${n.toFixed(1)}%` : "—";
  function bytes(n) {
    if (!valid(n)) return "—";
    let unit = 0; const names = ["B", "KiB", "MiB", "GiB", "TiB"];
    while (n >= 1024 && unit < names.length-1) { n /= 1024; unit++; }
    return `${n.toFixed(unit > 0 ? 1 : 0)} ${names[unit]}`;
  }
  const rate = n => valid(n) ? bytes(n) + "/s" : "—";
  const key = p => `${p.pid}:${p.start_time}`;
  const io = p => valid(p.disk_read_bps) || valid(p.disk_write_bps) ? (p.disk_read_bps || 0)+(p.disk_write_bps || 0) : null;
  function create() {
    root = node("dialog", "task-manager"); root.id = "task-manager"; root.setAttribute("aria-labelledby", "tm-title");
    root.innerHTML = `
      <header class="tm-header"><div><p class="tm-eyebrow">WEBTERM · LIVE SYSTEM</p><h2 id="tm-title">Task manager</h2></div>
        <div class="tm-header-actions"><span id="tm-live" class="tm-live" role="status">Refreshing…</span><button id="tm-refresh" type="button" class="tm-button">Refresh</button><button id="tm-close" type="button" class="tm-close" aria-label="Close task manager">×</button></div></header>
      <section id="tm-summary" class="tm-summary" aria-label="System resource summary"></section>
      <div class="tm-toolbar"><label class="tm-search-label"><span class="sr-only">Filter processes</span><input id="tm-search" type="search" placeholder="Filter by name, PID, user, path or port" autocomplete="off"></label><span id="tm-count" class="tm-muted"></span></div>
      <p id="tm-error" class="tm-error" role="alert" hidden></p>
      <div class="tm-body"><section class="tm-list" aria-label="Running processes"><div id="tm-scroll" class="tm-table-scroll"><table class="tm-table"><thead><tr>
        <th scope="col" data-column="name"><button type="button" data-sort="name">Process</button></th>
        <th scope="col" data-column="pid"><button type="button" data-sort="pid">PID</button></th>
        <th scope="col" data-column="cpu"><button type="button" data-sort="cpu">CPU</button></th>
        <th scope="col" data-column="memory"><button type="button" data-sort="memory">RAM</button></th>
        <th scope="col" data-column="disk"><button type="button" data-sort="disk">Disk I/O</button></th>
        <th scope="col" data-column="gpu"><button type="button" data-sort="gpu">GPU</button></th>
        <th scope="col" data-column="port"><button type="button" data-sort="port">Ports</button></th>
        <th scope="col" data-column="user"><button type="button" data-sort="user">User</button></th>
      </tr></thead><tbody id="tm-rows"></tbody></table><p id="tm-empty" class="tm-empty">Collecting processes…</p></div></section>
      <aside id="tm-detail" class="tm-detail" aria-label="Process details" hidden></aside></div>
      <footer class="tm-footer"><span id="tm-notice">CPU: 100% per logical core. Disk: read/write I/O. — means unavailable.</span><span>Refresh 5s · interaction pause 10s</span></footer>
      <div id="tm-tooltip" class="tm-tooltip" role="tooltip" hidden></div><div id="tm-port-menu" class="tm-port-menu" role="dialog" aria-label="Port actions" hidden></div>`;
    document.body.append(root);
    for (const id of ["live","refresh","close","summary","search","count","error","scroll","rows","empty","detail","notice","tooltip","port-menu"]) el[id] = root.querySelector(`#tm-${id}`);
    root.addEventListener("cancel", e => { e.preventDefault(); close(); });
    root.addEventListener("close", () => { if (state.open) close(); });
    el.close.addEventListener("click", close);
    el.refresh.addEventListener("click", () => { state.pauseUntil = 0; poll(true); });
    for (const event of ["pointermove","pointerdown","click","keydown","wheel"]) root.querySelector(".tm-body").addEventListener(event, pause, {passive:true});
    el.search.addEventListener("input", () => { state.search = el.search.value.toLowerCase().trim(); pause(); renderRows(); });
    root.querySelectorAll("[data-sort]").forEach(button => button.addEventListener("click", () => {
      const column = button.dataset.sort;
      state.direction = state.sort === column ? -state.direction : (["name","user","pid","port"].includes(column) ? 1 : -1);
      state.sort = column; pause(); renderRows();
    }));
    el.rows.addEventListener("pointerover", event => {
      const item = event.target.closest("[data-executable]");
      if (!item || !item.dataset.executable) return;
      el.tooltip.textContent = item.dataset.executable; el.tooltip.hidden = false; positionAbove(el.tooltip, item);
    });
    el.rows.addEventListener("pointerout", event => {
      const item = event.target.closest("[data-executable]");
      if (item && !item.contains(event.relatedTarget)) el.tooltip.hidden = true;
    });
    el.scroll.addEventListener("scroll", () => { el.tooltip.hidden = true; hidePort(); }, {passive:true});
    root.addEventListener("pointerdown", event => {
      if (!el["port-menu"].contains(event.target) && !event.target.closest("[data-port]")) hidePort();
    });
    document.addEventListener("visibilitychange", () => { if (state.open && !document.hidden) arm(0); });
    window.addEventListener("resize", () => { hidePort(); el.tooltip.hidden = true; });
  }
  function pause() {
    if (!state.open) return;
    state.pauseUntil = Date.now() + INTERACTION_DELAY;
    arm(INTERACTION_DELAY); renderLive();
  }
  function arm(delay = INTERVAL) {
    clearTimeout(state.timer);
    if (state.open) state.timer = setTimeout(() => poll(), Math.max(delay, state.pauseUntil-Date.now(), 0));
  }
  function renderLive() {
    if (!state.open) return;
    const remaining = Math.ceil((state.pauseUntil-Date.now())/1000);
    el.live.classList.toggle("is-paused", remaining > 0);
    el.live.textContent = remaining > 0 ? `Paused · ${remaining}s` : state.busy ? "Refreshing…" : "Live · every 5s";
  }
  async function poll(manual = false) {
    if (!state.open || state.busy) return;
    if (!manual && (document.hidden || Date.now() < state.pauseUntil)) { arm(); return; }
    state.busy = true; renderLive(); el.refresh.disabled = true;
    const controller = new AbortController(), generation = state.generation;
    state.controller = controller;
    const timeout = setTimeout(() => controller.abort(), 14000);
    try {
      const {body} = await state.request("/processes", {signal:controller.signal});
      if (!state.open || generation !== state.generation) return;
      if (!Array.isArray(body?.processes)) throw new Error("Invalid process metrics response");
      // Do not move a row under the pointer if interaction began during this request.
      if (!state.data || manual || Date.now() >= state.pauseUntil) {
        state.data = body; el.error.hidden = true; renderSummary(); renderRows();
      }
    } catch (error) {
      if (state.open && generation === state.generation) {
        el.error.textContent = error.name === "AbortError" ? "Metrics request timed out. The previous sample is retained." : error.message || "Metrics unavailable. Retrying.";
        el.error.hidden = false;
      }
    } finally {
      clearTimeout(timeout);
      if (state.controller === controller) { state.controller = null; state.busy = false; el.refresh.disabled = false; }
      renderLive(); if (state.open && generation === state.generation) arm();
    }
  }
  function summaryCard(label, value, note) {
    const box = node("div", "tm-summary-card"); box.append(node("span", "tm-summary-label", label), node("strong", "", value));
    if (note) box.append(node("small", "", note));
    return box;
  }
  function renderSummary() {
    const d = state.data; if (!d) return;
    const cards = [summaryCard("CPU", percent(d.cpu_percent), `${d.logical_cpus || "—"} logical cores`),
      summaryCard("Memory", bytes(d.memory_used_bytes), `${bytes(d.memory_total_bytes)} total`)];
    const disk = (d.disks || [])[0];
    cards.push(summaryCard("Disk " + (disk?.path || ""), disk ? bytes(disk.used_bytes) : "—", disk ? `${bytes(disk.available_bytes)} available` : "Unavailable"));
    if (d.gpu_devices?.length) {
      for (const gpu of d.gpu_devices) cards.push(summaryCard(gpu.name, percent(gpu.utilization_percent), `${bytes(gpu.memory_used_bytes)} / ${bytes(gpu.memory_total_bytes)} VRAM`));
    } else cards.push(summaryCard("GPU", "Not reported", "No supported driver counters available"));
    el.summary.replaceChildren(...cards);
    const date = new Date((d.timestamp || Date.now()/1000)*1000);
    el.notice.textContent = `Sample ${date.toLocaleTimeString()} · CPU: 100% per core · Disk: I/O · — unavailable${d.truncated ? " · Process list capped" : ""}`;
  }
  function sortValue(p, column) {
    switch (column) {
      case "name": return p.name || "";
      case "pid": return p.pid;
      case "user": return p.user || "";
      case "cpu": return p.cpu_percent;
      case "memory": return p.memory_bytes;
      case "disk": return io(p);
      case "gpu": return p.gpu_percent;
      case "port": return p.ports?.length ? Math.min(...p.ports.map(p => p.port)) : null;
      default: return p.pid;
    }
  }
  function filteredRows() {
    return (state.data?.processes || []).filter(p => !state.search || [p.name,p.pid,p.user,p.exe,...(p.ports || []).map(p => p.port)].join(" ").toLowerCase().includes(state.search)).slice().sort((a,b) => {
      const av = sortValue(a,state.sort), bv = sortValue(b,state.sort);
      if (av === null || av === undefined) return bv === null || bv === undefined ? a.pid-b.pid : 1;
      if (bv === null || bv === undefined) return -1;
      const compared = typeof av === "string" ? av.localeCompare(String(bv)) : av-bv;
      return compared * state.direction || a.pid-b.pid;
    });
  }
  function renderRows() {
    if (!state.data) return;
    hidePort(); el.tooltip.hidden = true;
    const list = filteredRows(), scroll = el.scroll.scrollTop;
    const focus = document.activeElement?.closest("[data-process-pid]")?.dataset.processPid;
    const fragment = document.createDocumentFragment();
    for (const process of list) {
      const tr = node("tr"); tr.dataset.processPid = String(process.pid); tr.dataset.identity = key(process);
      tr.classList.toggle("is-selected", state.selected && key(process) === key(state.selected));
      const nameCell = node("td", "tm-name-cell");
      const name = node("button", "tm-process-name", process.name || `Process ${process.pid}`); name.type = "button";
      name.dataset.executable = process.exe || process.name || "Path unavailable";
      name.setAttribute("aria-label", `Details for ${process.name}, PID ${process.pid}`); nameCell.append(name); tr.append(nameCell);
      for (const [cls,text] of [["tm-number",process.pid],["tm-number",percent(process.cpu_percent)],["tm-number",bytes(process.memory_bytes)]]) tr.append(node("td",cls,text));
      const disk = node("td","tm-number"); disk.append(node("span","",rate(io(process))), node("small","tm-submetric",`R ${rate(process.disk_read_bps)} · W ${rate(process.disk_write_bps)}`)); tr.append(disk);
      const gpu = node("td","tm-number"); gpu.append(node("span","",percent(process.gpu_percent)), node("small","tm-submetric",bytes(process.gpu_memory_bytes))); tr.append(gpu);
      const ports = node("td","tm-ports");
      if (!(process.ports || []).length) ports.textContent = "—";
      for (const port of (process.ports || []).slice(0,8)) ports.append(portButton(port));
      if ((process.ports || []).length > 8) ports.append(node("small","tm-muted",`+${process.ports.length-8}`));
      tr.append(ports,node("td","tm-user",process.user || String(process.uid)));
      tr.addEventListener("click", event => { if (!event.target.closest("[data-port]")) selectProcess(process); });
      fragment.append(tr);
    }
    el.rows.replaceChildren(fragment); el.scroll.scrollTop = scroll;
    if (focus) el.rows.querySelector(`[data-process-pid="${Number(focus)}"] .tm-process-name`)?.focus({preventScroll:true});
    el.empty.hidden = list.length > 0; el.empty.textContent = "No matching processes.";
    el.count.textContent = `${list.length} of ${state.data.total_processes ?? state.data.processes.length} processes`;
    root.querySelectorAll("[data-column]").forEach(th => {
      const active = th.dataset.column === state.sort;
      th.setAttribute("aria-sort", active ? (state.direction === 1 ? "ascending" : "descending") : "none");
      th.classList.toggle("is-sorted",active);
    });
  }
  function portButton(port) {
    const button = node("button", "tm-port", `${port.port}${port.protocol === "udp" ? "/udp" : ""}`);
    button.type = "button"; button.dataset.port = String(port.port);
    button.title = `${port.address || "loopback"}:${port.port} (${port.protocol})`;
    button.addEventListener("click", event => { event.stopPropagation(); pause(); showPort(port,button); });
    return button;
  }
  function positionAbove(popup, target) {
    const rect = target.getBoundingClientRect(), width = popup.offsetWidth, height = popup.offsetHeight;
    popup.style.left = `${Math.max(8,Math.min(innerWidth-width-8,rect.left))}px`;
    popup.style.top = `${Math.max(8,rect.top-height-8)}px`;
  }
  function hidePort() { if (el["port-menu"]) el["port-menu"].hidden = true; }
  function showPort(port,button) {
    const popup = el["port-menu"]; popup.replaceChildren();
    const url = location.hostname === "9992-colabdev.alima.freeddns.org"
      ? `https://${Number(port.port)}-proxy-colabdev.alima.freeddns.org/` : `/?proxyport=${Number(port.port)}`;
    popup.append(node("strong","",`Port ${port.port} · ${port.protocol.toUpperCase()}`), node("code","",url));
    if (port.protocol === "tcp") {
      const link = node("a","tm-button","Open HTTP preview"); link.href = url; link.target = "_blank"; link.rel = "noopener noreferrer"; popup.append(link);
    } else popup.append(node("small","tm-muted","UDP is listed for inspection; this URL proxy serves HTTP/WebSocket."));
    const copy = node("button","tm-button","Copy URL"); copy.type = "button";
    copy.addEventListener("click", async () => {
      pause();
      try { await navigator.clipboard.writeText(new URL(url,location.origin).href); copy.textContent = "Copied"; }
      catch (_) { copy.textContent = "Select the URL above to copy"; }
    });
    popup.append(copy); popup.hidden = false; positionAbove(popup,button);
  }
  async function selectProcess(process) {
    pause(); hidePort(); state.selected = {...process}; state.action = null; renderRows(); renderDetail(process,true);
    state.detailController?.abort(); const controller = new AbortController(); state.detailController = controller;
    const generation = state.generation, identity = key(process);
    try {
      const {body} = await state.request(`/processes/${process.pid}?start_time=${encodeURIComponent(process.start_time)}`,{signal:controller.signal});
      if (state.open && generation === state.generation && state.selected && key(state.selected) === identity) {
        state.selected = {...process,...body}; renderDetail(state.selected,false);
      }
    } catch(error) {
      if (error.name !== "AbortError" && state.open && generation === state.generation && state.selected && key(state.selected) === identity) {
        renderDetail({...process,can_control:false},false,error.message || "Process details unavailable.");
      }
    }
  }
  function detailPair(grid,label,value) { grid.append(node("dt","",label),node("dd","",String(value ?? "—"))); }
  function renderDetail(p,loading,error) {
    const pane = el.detail; pane.hidden = false; pane.replaceChildren();
    const header = node("div","tm-detail-header"), title = node("div"); title.append(node("p","tm-eyebrow",`PROCESS ${p.pid}`),node("h3","",p.name));
    const closeButton = node("button","tm-close","×"); closeButton.type = "button"; closeButton.setAttribute("aria-label","Close process details");
    closeButton.addEventListener("click",() => {state.detailController?.abort();state.selected = null;pane.hidden = true;renderRows();});
    header.append(title,closeButton);pane.append(header);
    if (loading) pane.append(node("p","tm-muted","Loading details…"));
    if (error) pane.append(node("p","tm-error",error));
    pane.append(node("h4","","Executable"),node("code","tm-path",p.exe || "Path unavailable"));
    if (p.cwd) pane.append(node("h4","","Working directory"),node("code","tm-path",p.cwd));
    const grid = node("dl","tm-detail-grid");
    detailPair(grid,"User",p.user);detailPair(grid,"Parent PID",p.ppid);detailPair(grid,"State",p.state);
    detailPair(grid,"CPU",percent(p.cpu_percent));detailPair(grid,"Memory",bytes(p.memory_bytes));detailPair(grid,"Threads",p.threads);
    detailPair(grid,"Disk read",bytes(p.read_bytes));detailPair(grid,"Disk written",bytes(p.write_bytes));
    detailPair(grid,"GPU",percent(p.gpu_percent));detailPair(grid,"GPU memory",bytes(p.gpu_memory_bytes));
    detailPair(grid,"Open descriptors",p.open_fds);detailPair(grid,"Elapsed",valid(p.elapsed_s) ? `${Math.round(p.elapsed_s)} seconds` : "—");
    pane.append(grid);
    if (p.ports?.length) { const ports = node("div","tm-detail-ports");for (const port of p.ports) ports.append(portButton(port));pane.append(node("h4","","Listening ports"),ports); }
    const priority = node("div","tm-priority"), label = node("label","","Priority (nice)");label.htmlFor = "tm-nice";
    const input = node("input");input.type="number";input.min="-20";input.max="19";input.step="1";input.value=String(p.nice ?? 0);input.id="tm-nice";
    const apply = node("button","tm-button","Apply priority");apply.type="button";
    for(const control of [input,apply])control.disabled=!p.can_control || loading;
    apply.addEventListener("click",() => { const nice=Number(input.value);if(!Number.isInteger(nice)||nice < -20||nice > 19){detailMessage("Priority must be an integer from -20 to 19.",true);return;}performAction("priority",nice); });
    priority.append(label,input,apply);pane.append(priority,node("p","tm-help","Higher nice values mean lower CPU scheduling priority. Raising priority may require administrator privileges."));
    const actions=node("div","tm-process-actions");
    for(const [kind,text] of [["terminate","Terminate"],["kill","Force kill"]]){
      const button=node("button","tm-button tm-danger",text);button.type="button";button.dataset.action=kind;button.disabled=!p.can_control||loading;
      button.addEventListener("click",()=>confirmAction(kind));actions.append(button);
    }
    pane.append(actions);
    if(!p.can_control)pane.append(node("p","tm-help","Controls are disabled for protected WebTerm/system processes and processes owned by another user."));
    const confirm=node("div","tm-confirm");confirm.id="tm-confirm";confirm.hidden=true;pane.append(confirm);
    const message=node("p","tm-detail-message");message.id="tm-detail-message";message.setAttribute("role","status");pane.append(message);
    pane.append(node("p","tm-help","Actions verify PID and start time. Environment variables and command-line secrets are not exposed."));
  }
  function detailMessage(message,error=false){const box=root.querySelector("#tm-detail-message");if(box){box.textContent=message;box.classList.toggle("tm-error",error);}}
  function confirmAction(kind){
    pause();state.action=kind;const box=root.querySelector("#tm-confirm");box.hidden=false;box.replaceChildren();
    box.append(node("p","",`${kind === "kill" ? "Force kill" : "Terminate"} ${state.selected.name} (PID ${state.selected.pid})? Unsaved work may be lost.`));
    const yes=node("button","tm-button tm-danger","Confirm "+(kind === "kill" ? "force kill" : "terminate"));yes.type="button";yes.id="tm-confirm-action";yes.addEventListener("click",()=>performAction(kind));
    const no=node("button","tm-button","Cancel");no.type="button";no.addEventListener("click",()=>{box.hidden=true;state.action=null;});box.append(yes,no);
  }
  async function performAction(action,nice){
    if(state.actionBusy || !state.selected?.can_control)return;
    const process={...state.selected},generation=state.generation;state.actionBusy=true;
    el.detail.querySelectorAll("button,input").forEach(n=>n.disabled=true);
    try{
      const {body}=await state.request(`/processes/${process.pid}/action`,{method:"POST",json:{start_time:process.start_time,action,...(action === "priority" ? {nice}: {})}});
      if(!state.open || generation!==state.generation || !state.selected || key(state.selected)!==key(process))return;
      if(action === "priority"){state.selected.nice=body.nice;renderDetail(state.selected,false);detailMessage(`Priority is now ${body.nice}.`);}
      else {state.selected.can_control=false;renderDetail(state.selected,false);detailMessage(`${action === "kill" ? "SIGKILL" : "SIGTERM"} sent. The list will refresh after the interaction pause.`);}
      pause();
    }catch(error){if(state.open && generation===state.generation){renderDetail(state.selected,false);detailMessage(error.message||"Process action failed.",true);}}
    finally{state.actionBusy=false;}
  }
  function open(options){
    if(state.open)return;
    if(!root)create();state.open=true;state.request=options.request;state.trigger=options.trigger;
    state.data=null;state.selected=null;state.search="";state.pauseUntil=0;state.busy=false;state.generation++;
    el.search.value="";el.rows.replaceChildren();el.summary.replaceChildren();el.detail.hidden=true;el.error.hidden=true;el.empty.hidden=false;el.empty.textContent="Collecting processes…";
    root.showModal();el.search.focus({preventScroll:true});state.ticker=setInterval(renderLive,1000);poll();
  }
  function close(){
    if(!state.open)return;state.open=false;state.generation++;clearTimeout(state.timer);clearInterval(state.ticker);
    state.controller?.abort();state.detailController?.abort();state.controller=null;state.detailController=null;state.busy=false;
    state.data=null;state.selected=null;hidePort();el.tooltip.hidden=true;el.rows.replaceChildren();el.summary.replaceChildren();el.detail.replaceChildren();
    if(root.open)root.close();state.trigger?.focus?.({preventScroll:true});
  }
  window.WebTermMonitor = Object.freeze({open,close});
})();
