# webterm

`webterm` is a compact Rust service and CLI for folder-backed workspaces and persistent named terminals. The service binds to loopback only and is published through the TLS-only Caddy route in [`deploy/7681-webterm.caddy`](deploy/7681-webterm.caddy).

## Build and test

```sh
cargo test --locked -j 1
cargo build --locked -j 1
```

Copy `deploy/webterm.toml.example` to `/etc/webterm/`, create a mode-0600 token containing at least 24 random characters at `/etc/webterm/token`, install the binary at `/usr/local/bin/webterm`, and install `deploy/webterm-runtime.service` plus `deploy/webterm.service`. The native runtime has its own cgroup and owns every newly created PTY, so restarting the HTTP service only disconnects clients and does not kill their shells.

`deploy/webterm-tmux.service` and `deploy/tmux.conf` are transitional compatibility for the two already-running `wt-` sessions. Keep that existing service running until those sessions are deliberately retired; WebTerm does not migrate their live processes, restart them, or create new tmux sessions. Do not restart the legacy service as part of this upgrade.

```sh
webterm --config /etc/webterm/webterm.toml config
curl http://127.0.0.1:10000/health
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:10000/api/v1/status
```

The same protected bearer token enables the native stateless MCP Streamable
HTTP endpoint at `https://your-webterm-host/mcp` (with `/mcp/` accepted as an
alias). Configure the token through `auth_token_file` in the WebTerm TOML file
or `WEBTERM_AUTH_TOKEN`; never put it in the endpoint URL. MCP does not accept
the browser session cookie.

```sh
curl https://your-webterm-host/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

The server returns JSON responses and supports MCP `2025-11-25`, `2025-06-18`,
and `2025-03-26`. It does not implement OAuth or the legacy SSE transport, so
use a client that can attach a static bearer token to Streamable HTTP requests.
Not every MCP client supports this combination. See
[`docs/http-api.md`](docs/http-api.md#mcp-streamable-http) for the protocol and
tool contract.

Install `deploy/7681-webterm.caddy` in the Caddy configuration and validate before reloading. The policy deliberately permits inline styles because xterm.js computes terminal geometry with runtime style attributes; scripts remain restricted to same-origin vendored files. Do not add proxy Basic Auth: the application provides the required password-only login and session/CSRF controls.

Configuration can be selected with `--config` or `WEBTERM_CONFIG`. `WEBTERM_LISTEN`, `WEBTERM_DATABASE_PATH`, `WEBTERM_RUNTIME_SOCKET`, `WEBTERM_TMUX_SOCKET`, and `WEBTERM_AUTH_TOKEN` are environment overrides. The native runtime socket must be an absolute private Unix-socket path distinct from the database and legacy tmux socket. The default listen address is `0.0.0.0:10000` (all interfaces).

## Browser authentication

The development service currently starts with `--passwd 2208` as explicitly requested for testing. Existing `?passwd=2208` authentication compatibility remains available where supported by the HTTP/MCP baseline. This exposes a development credential in the systemd unit/process arguments and URL query strings and **must not be used in production**. The browser login path hashes the configured password with Argon2id at startup and keeps only the hash for verification.

For production, remove `--passwd`, generate an Argon2id PHC string using a trusted offline password tool, store it mode 0600, and set `web_password_hash_file` to that protected path (systemd credentials are preferred). Restarting the service invalidates all in-memory browser sessions. Rotate the separate bearer token independently.

Browser sessions use a Secure, HttpOnly, SameSite=Strict cookie, a fixed expiry, same-origin enforcement, CSRF tokens for logout/mutations, and bounded login rate limiting. See [`docs/http-api.md`](docs/http-api.md) for the stable v1 contract.

## Workspaces

Workspace folders must exist and resolve inside one of the canonical `workspace_roots`. The CLI never creates or deletes the folder itself. Names and paths are unique.
Names cannot be purely numeric because numeric selectors refer to record IDs.

```sh
webterm --config /etc/webterm/webterm.toml workspace add webterm /home/admin/project/webterm
webterm --config /etc/webterm/webterm.toml workspace list
webterm --config /etc/webterm/webterm.toml workspace show webterm --json
webterm --config /etc/webterm/webterm.toml workspace update webterm --name webterm-dev
webterm --config /etc/webterm/webterm.toml workspace remove webterm-dev
```

All list/show commands offer `--json` for scripts and future clients. Data is stored in SQLite with WAL mode and transactional embedded migrations.

## Terminals

Each terminal has a unique name within its workspace and runs in the workspace folder. New records receive a `pty-` session ID and are created only by the native runtime daemon through its mode-0600 Unix socket. CLI, TUI, HTTP, WebSocket, and MCP processes are clients and never spawn shells. Historical `wt-` IDs are routed only to already-running legacy tmux sessions; a stopped historical record is never recreated in tmux.

```sh
webterm --config /etc/webterm/webterm.toml terminal create webterm shell
webterm --config /etc/webterm/webterm.toml list
webterm --config /etc/webterm/webterm.toml terminal write webterm shell --data 'printf hello' --enter
webterm --config /etc/webterm/webterm.toml terminal capture webterm shell
webterm --config /etc/webterm/webterm.toml terminal resize webterm shell --columns 120 --rows 40
webterm --config /etc/webterm/webterm.toml terminal attach webterm shell
webterm --config /etc/webterm/webterm.toml terminal stop webterm shell
```

Run `webterm --config /etc/webterm/webterm.toml tui` (or omit the command) for the keyboard-and-mouse hierarchy. Arrow keys or `j`/`k` select rows, Enter attaches, clicking a workspace expands it, and clicking a running terminal attaches it. In a native raw attach, press Ctrl-] to detach without exiting the shell. `q` exits the hierarchy.

## Native runtime limits

The daemon exposes only a private Unix socket (mode 0600); child shells retain ordinary network access. It owns up to 32 native terminals and 16 attached viewers per terminal. Screen dimensions are bounded to 512 columns by 256 rows, with 500 retained history rows. Current-screen ANSI state is preserved on reconnect; older history is replayed as plain text. Slow viewers are disconnected without blocking the terminal, and blocked input requests time out rather than holding the entire runtime.

The default login shell restarts after exit with a one-second delay and no command replay. Use Stop/Delete to permanently close a terminal. Local CLI attachment uses Ctrl-] to detach and restores the caller's TTY settings.

## Terminal lifetime and mobile status

The bottom status strip contains host CPU/RAM and terminal shortcuts, with horizontal scrolling on narrow screens. Authenticated metrics refresh every five seconds, requests time out after ten seconds, and unavailable/stale samples are labelled. The workspace header displays the actual path, starts at its end and preserves a manual scroll position. The app follows the visual viewport so the terminal and footer fit above the on-screen keyboard.

Browser disconnects, mobile backgrounding and HTTP-service restarts detach clients rather than stop native PTYs. WebSockets use bounded I/O, and the runtime maintains the canonical terminal screen/history independently of viewers. Use the explicit Stop/Delete action for permanent termination. Native processes do not survive a host or native-runtime failure.

Native resize is runtime-owned and broadcast to all viewers with a canonical snapshot. Legacy tmux windows resume automatic sizing to the latest client. An inset xterm host prevents the final row from being clipped behind the footer. Deploy the supplied HTTP unit without `ProcSubset=pid` so host metrics are readable; keep the legacy tmux service untouched while either transitional session remains. Reload Caddy only after validating the complete active configuration. Browser sessions are in memory, so an HTTP-service restart requires signing in again.

## Native runtime end-to-end verification

After the debug build, run the isolated native-only suite with the installed browser-test environment:

```sh
PLAYWRIGHT_BROWSERS_PATH="$PWD/tests/web/.browsers" \
  tests/web/.venv/bin/python tests/native_runtime_e2e_GPT-6-Astra-Pro_ChatGPT.py
```

It creates disposable data and services, deliberately makes `tmux` fail, and verifies native CLI/MCP creation, browser rendering, shared resize, replay, backpressure, HTTP-restart persistence, CLI detach/TTY restoration, Ctrl-C, shell restart, and permanent Stop. It never uses the production database.

## Path-based MCP command workflow

MCP initialization directs clients to use `bash` and `python` for builds, tests and
debugging. `workspace_id` is an absolute folder path within configured workspace
roots. First execution registers it once (canonical aliases share the same record).
The browser tree displays folder paths and refreshes every five seconds. Browser
HTTP routes retain numeric database IDs internally. Numeric MCP workspace IDs are rejected; refresh client schemas to use paths.

`bash` / `python` accept `workspace_id`, `command` / `code`, `wait_s=20` (0–20),
and `full_output=false`. Execution uses a real PTY and continues after the response.
Completed commands return output and exit code; long commands return a running
terminal handle. `terminal_read` and `terminal_capture` retrieve retained command
output. Every terminal operation advertises the workspace folder argument.

Above 2000 Unicode characters, default output is exactly first 500 plus last 1500,
with omitted-count metadata. Prefer `full_output=false`; true bypasses preview,
not backend retention. Command records retain first 65536 + last 196608 characters;
ordinary capture is limited to selected lines (up to 1000) / 256 KiB. The private
`mcp-commands` state directory survives frontend restarts. Temporary source files
are removed on completion. Explicit user build logs belong in the workspace/build
directory rather than these runtime records.

For requested public development previews, clients are instructed to prefer
`cloudflared tunnel --url http://127.0.0.1:PORT` in a second foreground terminal,
never to expose private files or admin/MCP services. No tunnel starts on connect.

Regression checks: `cargo test --locked`, `python3 tests/command_runner_test.py`,
and the isolated live MCP workflow under `.agentwork/workspace-v2/`.

Operators may set `WEBTERM_COMMAND_DIR` to an absolute private state folder. Colab
sets `/build/webterm-commands` so command source/output stay outside persisted home.

## Runtime port proxy and task manager

`/path?proxyport=3000&x=1&x=2` forwards the unchanged escaped path and query to
`http://127.0.0.1:3000/path?x=1&x=2` on the machine running WebTerm. The Colab
gateway first forwards to Colab WebTerm, so its port is always inside `dev`,
not on the Alibaba gateway. Authenticate with the existing WebTerm browser
session or MCP bearer token. Opening a port URL does not allocate a Colab VM.

HTTP methods and streamed request/response bodies, SSE and WebSocket frames
(including binary frames, subprotocols and close codes) are forwarded. The
existing Caddy HTTPS edge accepts HTTP/1.1, HTTP/2 and HTTP/3. The internal proxy
hop uses HTTP/1.1; this is not a raw UDP/QUIC tunnel or an HTTP/3-only upstream
client. Targets are fixed to loopback; the WebTerm control port and CONNECT are
rejected. WebTerm/MCP credentials are stripped before forwarding; app cookies
retain their original paths and local redirects carry the selected port.

Root-relative assets keep their original paths and inherit the selected-port
cookie. The proxy does not rewrite arbitrary HTML or JavaScript. Use separate
cloudflared origins for independent simultaneous previews. **Only proxy trusted
applications:** query previews share the WebTerm origin and are not a security
boundary. Public tunnel URLs also require care around private data. Caddy does
not impose the control-page Content Security Policy on proxied applications.

Click CPU or RAM in the terminal footer to open Task manager. It shows process
name, PID, per-core CPU, resident memory, disk read/write I/O, ports, owner and
available GPU data. Sort any column or filter by name/path/PID/port. Hover a name
for its full executable path; click it for details, priority and explicit
terminate/force-kill confirmation. Port buttons offer Copy URL and Open HTTP
preview. UDP listeners are displayed, but cannot be opened as HTTP previews.

Samples refresh every five seconds. Pointer movement, clicks, keyboard input or
scrolling inside the process panel postpone automatic refresh for ten seconds;
an in-flight result cannot move a row under the pointer. Manual refresh remains
available. Failed samples retain the previous view, and logout closes/clears it.

The Linux collector uses `/proc`, NVIDIA `nvidia-smi`, and Intel/AMD DRM sysfs and
fdinfo when the driver exposes them. Unsupported or permission-restricted
measurements are null (shown as an em dash), not invented zeros. Per-process
CPU can exceed 100% when using multiple cores. Disk columns represent I/O, not
files attributed to a process. Intel/AMD support is parser/fixture-tested;
physical vendor validation depends on the available host hardware.

Process actions require browser authentication, same-origin plus CSRF protection,
and the exact PID/start-time identity. Only the service user's own non-WebTerm
processes can be controlled; raising scheduling priority may be denied by the OS.
Kill uses pidfds where available. Executable/cwd are shown; environment variables
and full command-line arguments are deliberately not exposed.

Tests: `cargo test --locked`, `python3 tests/process_monitor_test.py`,
`tests/web/.venv/bin/python tests/web/test_process_monitor.py`. Set
`WEBTERM_BROWSER` to an installed Chromium executable for browser tests.
The disposable integration harness is `.agentwork/proxy-monitor/live_proxy.py`.

## Query-selected previews and task metadata (v3)

Open `/app/index.html?proxyport=3000&other=value` to reach
`http://127.0.0.1:3000/app/index.html?other=value` in this WebTerm runtime.
The routing parameter is examined before ALL route handlers, including `/`,
`/log`, `/mcp`, `/assets` and `/api`. Only `proxyport` is removed. Other query
parameters, duplicates, percent encodings, the request method/body, and path
remain intact. WebSocket upgrades use the same routing and support explicit
`/socket?proxyport=3000`. Generated port-popup links now use `/?proxyport=PORT`.
Legacy `/proxy/PORT/` URLs remain a compatibility route, not the preferred UI.

Because browsers do not propagate document queries to `/resource` URLs, a
successful authenticated preview sets an HttpOnly, SameSite selection cookie.
HTTP requests prefer an explicit `proxyport`, then a same-origin referrer with
that parameter, then the selected-port cookie. `/?proxyport=0` clears selection
and returns to the control UI. Control clients set `X-WebTerm-Control: 1` to
ignore inherited selection; explicit query parameters still take priority.
Control-terminal WebSocket URLs explicitly use `proxyport=0` without clearing
selection. Auth cookies and bearer credentials are never forwarded to an app.

This is a SHARED-ORIGIN trusted-development preview, not an isolation boundary.
There is one implicit active port per browser origin. For simultaneous preview
ports, use explicit routing parameters for requests/WebSockets, separate browser
profiles, or separate cloudflared origins. Arbitrary third-party/untrusted apps
must not be hosted alongside the authenticated control UI. No HTML/JavaScript
rewriting is used: root assets, modules, fetch, streaming, gzip and WebSockets
work through unchanged paths. Native loopback HTTP remains HTTP/1.1; the Caddy
edge continues serving HTTP/2 and HTTP/3 clients.

`bash`, `python`, `terminal_write`, `terminal_read`, and `terminal_capture` now
require `task` (a simple single-line name, at most 80 characters) and `summary`.
Example: `task: "Build preview"`, `summary: "35/100 Testing resource routing"`.
The summary starts with 0..100/100 and has fewer than 20 description words.
All MCP workspace selectors are absolute folder paths only; numerical workspace
IDs and missing workspace arguments are rejected before execution. Browser
internal database references do not change this MCP contract.


## Compact MCP contracts (v4)

Colab `status`, `start`, `stop`, and `backup` return the same compact runtime view:
`session`, `running`, `kind`, `phase`, `webterm_ready`, `operation_active`, and an
optional short `error`. A disconnected/uncertain allocation has `running: null`,
not a fabricated yes/no. Native WebTerm `status` returns only service/runtime
readiness and workspace/running-terminal counts. Diagnostic detail is opt-in on
Colab: `runtime_details(section="timing" | "backup" | "graphics" | "errors")`.
The internal controller still maintains its full operational state, but duplicate
raw metrics, polling counters, GPU test arrays, IDs and hashes are not returned.
After an explicit backup, wait for `operation_active: false`, then check
`runtime_details(section="backup").data.success` before stopping.

`terminal_read` and `terminal_capture` accept optional `filter_cmd`. Its Bash
program receives the **full retained snapshot** on standard input before the
model-output preview is applied, and runs with the workspace as its current
directory, separately from the original terminal. It never sends input to the
original command. Examples: `grep -i error`, `grep ERROR | tail -n 20`, or
`tail -c 500` (bytes). For Unicode characters use:
`python3 -c 'import sys; print(sys.stdin.read()[-500:],end="")'`.

The filter deadline is five seconds. It returns `filter_exit_code` separately
from the original command's `exit_code`; grep with no matches returns 1 and an
empty output without falsely failing the original command. `filter_stderr`,
`filter_timed_out` and limit flags are included when relevant. Filtered stdout is
bounded by the retained-output limit, stderr by 2,000 characters; excessive
stream production is stopped. Filters have the runtime user's shell permissions,
not an isolation sandbox. Do not execute untrusted filter programs.

Whole retained text means up to the existing 262,144 command characters, not
previously discarded history. A filter receives `filter_input_limited: true`
when command retention was exceeded. Ordinary terminals use their retained
screen/capture buffer (up to 1,000 requested lines by default for filtering);
`capture_lines` indicates that scope. `full_output=false` still previews exactly
the first 500 and last 1,500 characters of a long **filter result**. Command text
is never re-executed to filter it. Repeated PID/timestamp/limit-description fields
are removed from terminal read responses.

The Colab `/log` sort menu defaults to date/newest first and also supports input
size and output size, largest first. Sizes are character counts of the complete
stored JSON payloads, consistent with the expand labels, not network transfer
bytes. Sorting runs on the server across retained history, preserves task and
workspace filters, and uses size/ID cursor ties for Load more. Full payloads remain
folded and lazy-loaded. Existing history receives size metadata during migration.

Every native and Colab MCP tool advertises a concrete `outputSchema`. Successful
Colab results are validated before delivery; a validation failure explicitly says
not to automatically repeat a possibly completed operation. MCP annotations are
capability-accurate: status/list/diagnostic tools are read-only and non-destructive;
shell execution, shell-capable filter reads, terminal input and stopping retain
appropriate write/destructive hints. There is no standard MCP "normal"/"dangerous"
classification that replaces those hints, and client approval policy is separate.


## Port subdomains and native image content (v5)

Colab previews now use `https://3000-proxy-colabdev.alima.freeddns.org/`.
Every path and query on that hostname reaches port 3000 in **Colab**, including
`/`, root-relative assets, API routes and WebSockets. The hostname wins over all
legacy `proxyport` parameters and cookies. No application base-path rewrite is
needed, and two ports can be open in separate tabs simultaneously. The Colab task
manager generates these links. Legacy query/path routes remain compatibility
options; local host WebTerm port buttons still address the host, not Colab.

Caddy obtains per-host certificates on demand, not a partial-label wildcard.
Its loopback-only ask endpoint allows only valid 1024–65535 ports observed on
user-owned Colab TCP listeners, excluding control ports. The listener cache
refreshes every five seconds; first HTTPS use also waits for certificate issuance.
No arbitrary hostname is admitted. New names are capped at 10 per rolling day by
`COLAB_DE_PREVIEW_TLS_NEW_PER_DAY`; existing grants persist for renewal when dev
is stopped. DNS must resolve the preview names to the gateway. Failed previews
never allocate a VM. TLS/HTTP2/HTTP3 terminate at Caddy; HTTP/1.1 and upgraded
WebSockets continue over the existing reverse SSH connection to Colab WebTerm.

Preview access stays authenticated. Browser sign-in happens only on the main
Colab gateway, which exchanges a one-time, 60-second, port-bound ticket for a
four-hour Secure/HttpOnly host-only cookie. A preview cookie cannot authorize the
main MCP or a different port, and VM replacement invalidates it. Application
cookies have Domain removed but retain their own Path. Gateway credentials,
preview cookies and internal forwarding headers are removed before the app.
App Authorization headers are supported with an authenticated preview cookie.
Only `/.colab-preview/accept` is reserved on an app hostname. API clients may
supply the MCP bearer token directly to the preview URL. Keep private dev apps
behind this authentication; public sharing remains an explicit cloudflared action.

`get_image` is read-only and accepts `workspace_id` (absolute folder), `path`
(relative to the folder or an absolute path inside it), `task`, and `summary`.
It returns a native MCP `image` block plus small, schema-validated metadata:
path, MIME type, byte count and dimensions. The gateway preserves the image block
instead of wrapping it as text. PNG, JPEG, GIF and WebP are detected from bytes;
extensions are not trusted. Limits are 16 MiB encoded file bytes, 16384 pixels
per side, 32 megapixels and bounded decoding (first frame for animations).
Only regular files inside the allowed workspace are read; path/symlink escapes,
corrupt files and oversized images are rejected. Image decoding concurrency is
limited to two calls. No shell, new terminal, or file upload is involved.

Example: `get_image({"workspace_id":"/home/dev/project/app","path":"screenshot.png",
"task":"Inspect UI","summary":"80/100 Checking rendered page"})`.

Image data is sent in the native image block, never clipped by the 2000-character
text preview. `/log` retains metadata and an explicit binary-omission marker,
not megabytes of base64; regular tool input/output logging is unchanged.

## Workspace explorer, preview history, and MCP logs

The sidebar displays each workspace's folder name. Click the name to reveal its
absolute path and a copy control. The disclosure button controls expansion;
background MCP activity never reopens a collapsed workspace. A light-blue dot
indicates newly observed terminals until their terminal list is viewed.

Expanded workspaces default to Files. The folder/terminal icon switches views;
each expanded pane is 400 pixels high and independently scrollable. Folder trees
include hidden files, keyboard expansion/navigation, a filter for loaded names,
and paged loading (500 entries per request). Visible roots and expanded child
folders refresh every five seconds. Newly observed files appear first, including
recent files in directories larger than one page. Browsing does not create a
terminal; the tab-bar plus button remains available for explicit creation.

Click a file for a quick preview at the right, then click it again (or choose
Open in viewer) for the Viewer tab. The newest selection goes first; previous
preview cards remain below and can be collapsed or revisited. Each card displays
the absolute path, type, size, copy-path control, and download link. Text previews
are bounded to 256 KiB, with a visible truncation notice. Images, native browser
audio/video players, static HTML, and interactive GLB models are supported.
Media delivery supports HEAD and single byte ranges for seeking. Codec support
depends on the browser. HTML previews load relative workspace assets but disable
scripts, forms, and parent navigation. Use an application port preview for an
interactive HTML application. GLB rendering uses the locally vendored Apache-2.0
model-viewer package; the main application permits its WebAssembly without
permitting general JavaScript eval.

The header log icon opens MCP invocation history. `/log` opens the same view;
behind colab_de it is `/webterm/log`, separate from the gateway's own `/log`.
Search and filter by tool, task, workspace, or status; sort by date, duration,
input size, or output size. Input/output panels load on demand. Polling occurs
every five seconds and can be paused. Sign out is available inside this dialog.

Call history is stored beside the workspace database as `*.mcp-log.db`, mode
0600, using SQLite WAL. It retains 2,000 completed invocation records, records
running/error/success/interrupted states, and bounds large payloads. Known
configured credentials and sensitive structured keys are redacted, as are native
image payloads. As with any private command history, avoid putting credentials
in command text. Unfinished calls are marked interrupted after an HTTP-service
restart. Preview and log APIs require the existing browser session or bearer
credential; paths must remain inside a registered workspace and configured root.

New read-only HTTP routes:

- `GET /api/v1/files/list?workspace_id=<id-or-path>&path=<relative-or-absolute>&offset=0`
- `GET /api/v1/files/preview?workspace_id=<id-or-path>&path=<file>`
- `GET|HEAD /api/v1/files/raw/<workspace-id>/<relative-path>`
- `GET /api/v1/tool-logs?q=&workspace=&task=&status=&sort=date&offset=0`
- `GET /api/v1/tool-logs/<id>`

The live explorer regression suite starts its own HTTP server and native runtime
with temporary workspaces, synthetic media, and test-only credentials:

```sh
cargo build --locked
WEBTERM_BIN=/build/cargo-target/debug/webterm \
WEBTERM_BROWSER=/home/dev/.local/bin/chromium \
python -m pytest tests/web/test_explorer_live.py -q
```
