# Browser HTTP API (v1)

All endpoints are same-origin. Browser authentication uses the Secure, HttpOnly, `SameSite=Strict` `__Host-webterm_session` cookie. The password is sent only in the JSON login request. It is never returned or stored by the browser application.

## Authentication

- `GET /api/v1/session` returns `200` with `{"authenticated":true,"csrf_token":"…","expires_in_seconds":28800,"capabilities":{…}}` for a live session, otherwise `401` with `authenticated:false`.
- `POST /api/v1/login` accepts `{"password":"…"}` and returns the authenticated session document plus the cookie. It requires a matching `Origin` and `Host`. Failed attempts are limited to five per client per minute and 50 globally per minute.
- `POST /api/v1/logout` requires the session cookie, a matching `Origin`, and the session's `X-CSRF-Token`; it expires the server session and cookie.

Sessions are in memory, expire after the configured fixed TTL, and are invalidated on service restart. State-changing browser APIs must require both same-origin validation and `X-CSRF-Token`.

Bearer authentication remains separate at `GET /api/v1/status` for future CLI/MCP clients and never grants a browser cookie.

# MCP Streamable HTTP

The native MCP endpoint is `POST /mcp` (`POST /mcp/` is also accepted). It is
a stateless, JSON-response implementation of MCP Streamable HTTP and supports
protocol versions `2025-11-25`, `2025-06-18`, and `2025-03-26`. A supported
version requested by `initialize` is echoed; an unknown version is negotiated
to the newest supported version. Accepted notifications, including
`notifications/initialized`, return an empty `202 Accepted` response. This
server does not open a server-sent event stream: `GET /mcp` returns `405 Method
Not Allowed` with `Allow: POST`.

Every MCP request requires the bearer token loaded by `auth_token_file` in the
WebTerm TOML configuration (or by the `WEBTERM_AUTH_TOKEN` environment override):

```sh
curl https://webterm.example/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"1"}}}'
```

For baseline compatibility, a configured development web password is also
accepted as the single `passwd` query parameter (for example `?passwd=2208`).
Bearer tokens are never accepted in the URL, and browser session cookies do not
authenticate MCP. A missing or incorrect credential returns `401`; a server
without either configured credential returns `503`. Supplied browser `Origin`
values must match the request host; non-browser clients may omit `Origin`.
Responses are non-cacheable. POST bodies are capped at 128 KiB and must use
`application/json`. Streamable HTTP clients normally advertise both
`application/json` and `text/event-stream`; this server always chooses the JSON
response permitted by that transport.

The MCP server implements `initialize`, `notifications/initialized`, `ping`,
`tools/list`, and `tools/call`. Its bounded tools are:

- `status`
- `workspace_list` (includes existing terminal records)
- `terminal_list` (optional positive `workspace_id`)
- `terminal_create` (positive `workspace_id`, optional unique `name`, bounded dimensions)
- `terminal_capture` (at most 1,000 lines and a 256 KiB result)
- `terminal_write` (at most 64 KiB of literal UTF-8 data)
- `terminal_resize` (2..512 columns, 2..256 rows)
- `terminal_stop` (stops the native PTY or live legacy tmux session but retains the record as `stopped`)

Terminal results expose `session_id` and `backend` (`native-pty` or
`legacy-tmux`). Bounded list results also include `runtime_pid` when it is
available for a live native session. They do not scan the host process table.

This endpoint does not implement OAuth discovery/authorization, MCP sessions,
the legacy HTTP+SSE transport, or a standalone SSE listener. Configure clients
that support bearer-authenticated Streamable HTTP with the endpoint URL and
token. Client support varies; a client that requires OAuth or only supports the
legacy SSE transport is not compatible without an appropriate trusted gateway.

## Workspaces

`GET /api/v1/workspaces` requires the session cookie and returns:

```json
{
  "workspaces": [
    {
      "id": 1,
      "name": "webterm",
      "path": "/home/admin/project/webterm",
      "terminals": [
        {"id": 1, "workspace_id": 1, "name": "survivor", "session_id": "pty-…", "backend": "native-pty", "status": "running"}
      ]
    }
  ]
}
```

`GET /api/v1/folders` is the authenticated server-side folder picker. With no
query it starts at `~/project` when that folder is inside an allowed root;
otherwise it starts at the first configured root. Pass a previously returned
path as `?path=…` to navigate. The response contains only canonical,
allowed-root-confined directories, sorted by name and limited to 500 entries:

```json
{
  "current": "/home/admin/project",
  "selected_name": "project",
  "parent": null,
  "entries": [{"name":"webterm","path":"/home/admin/project/webterm"}],
  "truncated": false
}
```

Files and symlinks that escape the configured roots are never returned. Invalid,
unreadable, or out-of-root paths return a descriptive `400` response.

## Mutations

Every mutation requires the session cookie, a matching `Origin`, and `X-CSRF-Token` from the session response.

- `POST /api/v1/workspaces` with `{"name":"…","path":"/allowed/folder"}` returns `201 {"workspace":…}`.
- `PATCH /api/v1/workspaces/:id` with `{"name":"…"}` and/or `{"path":"…"}` returns the workspace wrapper.
- `DELETE /api/v1/workspaces/:id` returns 204. If terminal records exist it requires `?force=true`, which stops them before removing records. Folders are never removed.
- `POST /api/v1/workspaces/:id/terminals` with `{}` atomically chooses the first available positive integer name in that workspace. An explicit `{"name":"…","cols":80,"rows":24}` is also accepted. Dimensions default to 80×24.
- `POST /api/v1/workspaces/:id/ensure-terminal` idempotently returns an existing running terminal or starts the workspace fallback `term1`. A stopped `term1` record is restarted in place with a fresh `pty-` session ID; its database ID, name, and workspace relationship are preserved. Concurrent calls share a `starting` lease and cannot create duplicate fallback terminals. The response includes `created` and `restarted` booleans.
- `PATCH /api/v1/terminals/:id` with `{"name":"…"}` renames the record and returns the terminal wrapper.
- `DELETE /api/v1/terminals/:id` stops its native or live legacy session, removes the record, and returns 204.

All three session capabilities are `true` when these routes and the terminal socket are available.

## Terminal WebSocket

Connect to `/api/v1/terminals/:id/ws` with the session cookie and matching browser `Origin`. Input and frames are capped at 64 KiB. PTY input and output each use a bounded 32-message channel, so slow peers apply backpressure rather than unbounded memory growth.

Client text messages:

```json
{"type":"input","data":"ls\r"}
{"type":"resize","cols":120,"rows":40}
```

Raw binary client frames are also accepted as terminal input. Server control messages are JSON text:

```json
{"type":"snapshot","data":"recent visible terminal text"}
{"type":"status","status":"connected"}
{"type":"status","status":"detached"}
{"type":"error","message":"validation or transport error"}
```

Live terminal output is sent as binary frames so arbitrary byte streams and UTF-8 code points split across PTY reads are preserved. The initial captured snapshot remains a JSON text message.

Closing the socket removes only that viewer; the runtime-owned native PTY keeps running and can be reconnected. The server closes an upgraded socket with code `4401` when the browser session reaches its fixed expiry.

New and restarted terminals are native PTYs owned by the independent runtime
daemon. The two already-running `wt-` sessions remain routed to the legacy tmux
service only for transitional compatibility; their processes are not migrated
or restarted, and no new tmux sessions are created.

## Browser host metrics

`GET /api/v1/metrics` requires the browser session cookie and returns `cpu_percent`, `memory_used_bytes`, `memory_total_bytes`, and `memory_percent`. Fields are numeric when available and otherwise null. CPU is a delta of aggregate host counters, so the first reading is normally null. Memory usage is `MemTotal - MemAvailable`. A process-wide sampler limits proc reads to once per second; the UI requests updates every five seconds. This endpoint does not grant MCP bearer tokens browser-cookie access.

## Process monitor

`GET /api/v1/processes` returns timestamp, system CPU/memory, filesystem usage,
GPU devices and a bounded process list. Each row contains `pid`, `start_time`,
`name`, `exe`, `user`, `cpu_percent`, `memory_bytes`, `disk_read_bps`,
`disk_write_bps`, `gpu_percent`, `gpu_memory_bytes`, `ports` and `can_control`.
Missing counters are null. CPU uses interval deltas; the initial sample can be
null. Up to 4096 processes and 512 file descriptors per process are inspected,
and at most 2000 rows are returned, with `truncated` indicating the cap.

`GET /api/v1/processes/PID?start_time=TICKS` returns detail for that exact process.
`POST /api/v1/processes/PID/action` requires the same browser session, Origin and
CSRF header as other mutations:

```json
{"start_time":"123456","action":"priority","nice":10}
```

Other actions are `terminate` (SIGTERM) and `kill` (SIGKILL). Unknown parameters,
stale identities and foreign/protected processes are rejected. A higher nice
number reduces CPU scheduling priority; range is -20 to 19.

## Legacy prefix development proxy (compatibility only)

Prefer `/TAIL?proxyport=PORT&QUERY` for new clients. The legacy
`/proxy/PORT/TAIL?QUERY` route forwards to `127.0.0.1:PORT` on this WebTerm machine,
stripping only `/proxy/PORT`. The trailing-slash redirect preserves the query.
Use a browser session or bearer token. Cross-origin requests are rejected;
browser mutations require same-origin Origin. Streaming and WebSocket upgrades
are supported. WebTerm credentials are not forwarded. Root-relative redirects
and app cookie paths are scoped to the proxy prefix. Only trusted apps belong
on this shared origin; set the application's base path accordingly.

Caddy terminates client HTTPS/HTTP2/HTTP3; the internal HTTP hop is HTTP/1.1.
Ports must be integers 1–65535; the control port is forbidden. Arbitrary upstream
hostnames, CONNECT and raw UDP are not supported.

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
