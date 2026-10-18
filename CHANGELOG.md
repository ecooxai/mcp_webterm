# Changelog

## 0.2.10 - 2026-10-14

- Native macOS support (Apple silicon and Intel). `webterm runtime` and `webterm serve` build and run on macOS: descriptor paths use `F_GETPATH`, process cwd/command line/session lookups use libproc and `sysctl` instead of `/proc`, the task manager collector uses `ps` and `lsof`, and dashboard CPU/memory come from Mach host statistics. Per-process disk I/O and GPU columns are null on macOS. Linux behaviour is unchanged.
- Release assets add `webterm-macos-universal` (plus a `.tar.gz` bundle) alongside the Linux binary.

## 0.2.9 - 2026-10-14

- Plain-text output is now the default: `/mcp` behaves like `/mcp?output_type=text` (no `outputSchema`, no `structuredContent`; the command output then one `key: value` line per field). Clients that want structured results connect to `/mcp?output_type=json`.

## 0.2.8 - 2026-10-14

- The `webterm` and `get_image` `outputSchema` now declare every field the tools return (`text`, `exit_code`, `running`, `terminal_id`, `workspace_id`, `snapshot`, `chars`, ...). Results are made to match the schema before sending: null fields are dropped and any undeclared field is appended to `text`, so strict MCP clients (e.g. Gemini) no longer reduce results to `{}`. `webterm status` reports `workspace_count` instead of a numeric `workspaces`.
- `/mcp?output_type=text` returns every result as one plain-text content block, with no `outputSchema` or `structuredContent`: the command output first, then one `key: value` line per field (`exit_code: 0`, `terminal_id: 7`, ...). `get_image` keeps its image block and adds the metadata as text. `output_type=json` (default) keeps structured results.
- `deploy/webterm.service` no longer uses `PrivateTmp`, so `get_image` sees the same `/tmp` as terminal commands.

## 0.2.7 - 2026-10-11

- The `webterm` MCP tool requires `workspace_id` (absolute folder or registered workspace name). It is the cwd and scope, so `cmd` and `text` only need relative paths; `webterm new NAME` no longer needs a path. `get_image` accepts workspace names too.
- `/log`: one search box (also matches commands and paths) applied 2 s after typing, no Apply button; header text, `Open /log` and Sign out removed; pause/resume is an icon at the top right with the status and sort selects beside it. Sorting by largest output lists only `webterm` calls.
- Sidebar: a workspace selector replaces the Navigation/Workspaces heading and shows the current workspace; the list shows the active workspace. The file search sits in the folder top bar (no Files/Every 5s text) and search results list the newest files first.
- `workspace_roots` defaults to `["/"]`. The new-workspace picker opens at `$HOME`, can go up, has no name field, and selects the folder you open; the workspace is named after the folder.

## 0.2.6 - 2026-10-10

- `/log` shows the webterm `cmd` under each webterm call title and the file path under each `get_image` title.
- MCP instructions and tool schemas ask for an honest quality score `n/100` (100 = perfect) instead of a progress number.
- The server tracks each task's total running time (latest call minus first call) and call count; `/log` shows them on every call and in a recent-task bar that filters by task. A task idle for 5 minutes with no running call restarts as `NAME-2`, `NAME-3`, ...
- `get_image` returns JPEG quality 90 at the original resolution by default; `jpeg: false` returns the original file.
- Reloading the web app reopens the last workspace and terminal.
- The sign-in form hides as soon as the password is accepted; a saved password shows a connecting status instead of the form, and the file-preview worker no longer delays opening terminals.

## 0.2.5 - 2026-10-10

- Remove the 32-terminal native runtime limit; creation is bounded only by OS resources. The runtime raises its descriptor limit and uses 256 KiB session thread stacks.
- Hibernate terminals idle for 1 hour (`runtime_idle_seconds`, `WEBTERM_IDLE_SECONDS`): the screen/history moves to a private swap file in `$TMPDIR`, falling back to `/var/tmp` and the state directory when a folder is full or unusable. Idle prompt shells are ended and transparently restarted in the same folder on next input or view; terminals running jobs keep their processes.
- PTY readers block on a wake pipe instead of polling every 100 ms, so idle terminals use no CPU.
- Add batch `list`/`stats` runtime RPCs; workspace tree, MCP list/status and reconciliation make one runtime call instead of one per terminal. Add `webterm runtime-stats`.
- Add `tests/runtime_stress.py` (2000 terminals: CPU, RSS, hibernation, resume, cleanup).

## 0.2.4 - 2026-10-05

- Rebuild and publish the current WebTerm source for Linux x86_64 using GNU/glibc, with commit metadata and verified SHA-256 checksums.
- Retain the v0.2.3 feature set, including managed autoboot scripts and compact terminal output.

## 0.2.3 - 2026-10-01

- Start and watch top-level `~/project/autoboot/*.sh` files after a 10-second delay,
  with filename-named terminals, debounced same-ID restarts, and deletion cleanup.
- Persist autoboot ownership, prevent duplicate watchers, preserve live apps across
  frontend restarts, recover daemon loss, and seed a safe editable template once.
- Reconnect open browser tabs after script restarts and remove deleted autoboot
  terminals automatically without a manual workspace refresh.
- Ignore nested directories, symlinks, non-shell files, oversized/unreadable scripts,
  and retry transient startup failures without reserving duplicate terminals.
- Add isolated process, browser, static-release, and live-instance autoboot regressions.
- Package v0.2.3 for the normal GNU/glibc Linux ABI and record its GLIBC symbol requirement and shared libraries in build metadata.

## 0.2.2 - 2026-10-01

- Bound compact tool text to the first 200 and last 800 Unicode characters. Truncated results point to `webterm read ID --full`; run/python cannot flood responses with `--full`.
- Include stderr in the returned text, retain early failure diagnostics separately from noisy stdout, and preserve bounded diagnostics from filter/control helper crashes.

- Run Bash and Python with `cmd="webterm run"` or `cmd="webterm python"` and a separate `workspace` parameter. Do not repeat the path in cmd; conflicting legacy paths are rejected before execution.
- Return compact command output as `structuredContent.text`, without a duplicate serialized JSON block in `content`. Error messages and native image blocks are retained.
- Display log output once, including historical duplicate responses, and preserve the actual input arguments without a synthetic second workspace ID.
- Instruct AI clients to create descriptive task names once, reuse them across related calls and follow-up chats, and report honest current progress or quality rather than resetting each call to zero.
- Add regressions for workspace-only calls, log formatting, native image output, and Chrome log details. Make browser target startup and runtime-panel fixtures reliable.
- Build Linux x86_64 release artifacts with musl, verify that they have no ELF interpreter or shared-library dependencies, and publish a tarball, build metadata, and SHA-256 checksums.

### Compact-client migration

Read `structuredContent.text` instead of `structuredContent.output`. Successful compact results use `content: []`; clients relying only on a JSON fallback in `content` must consume `structuredContent`. Hidden legacy tools and the internal CLI/PTY protocol keep their existing output keys. Proxies that parse compact results must update their readers too.

## 0.2.1

- Publish Linux x86_64 releases as static musl binaries so they run on older glibc hosts.

## 0.2.0 - 2026-09-29

- Consolidate terminal/workspace MCP operations behind Bash-backed `webterm(cmd)` while keeping native image output.
- Add literal `text` payloads for `run`, `python`, and `write`, plus compact polling and terminal listing.
- Load a lightweight independent password screen before xterm and workspace assets.
- Make mobile terminal output long-press selectable with drag selection and Copy/Clear actions, even while application mouse reporting is enabled.
- Make workspace-name clicks expand/collapse and double-clicks open the workspace as active.
- Preserve persistent PTY sessions across frontend-only deployment and improve isolated browser/integration coverage.
