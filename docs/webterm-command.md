# Bash-backed WebTerm command tool

## Preferred payload interface

The `webterm` MCP tool accepts required `cmd` plus optional **`text`**:

```json
{"cmd":"webterm run /home/dev/project/app","text":"printf '%s\n' 'hello world'\nprintf '%s\n' '$HOME stays literal in this quote'"}
{"cmd":"webterm write 123 --enter","text":"npm test"}
{"cmd":"webterm python /home/dev/project/app","text":"print('hello')"}
```

With `text`, `cmd` is one literal native run/python/write header, with its existing flags and optional leading `webterm`. Payload bytes travel through a private temporary file, not outer-shell interpolation or expanded argument lists. Multiline code, quotes, Unicode and leading/trailing whitespace are preserved. Code executes only in its intended run/python process; write input is sent only to the chosen terminal. Write still requires `--enter` to press Enter. Empty write text is allowed, including an Enter-only request; empty run/python code is rejected.

Do not supply code in both `cmd` and `text`, and do not put pipelines in a text-bearing header. Leave `text` out to use normal Bash pipelines such as `webterm read 123 | grep error`. Existing inline-code calls remain compatible. Text is limited to 32 KiB of UTF-8 for run/python or 64 KiB for write, subject also to the existing total JSON request limit. NUL is rejected. Invalid types, unsupported commands, conflicting payloads and invalid headers fail before a terminal is created.

The Colab gateway forwards both fields unchanged and includes `text` in its offline fallback schema. Native IDs, tracking, asynchronous handles, output limits and pipe behavior are unchanged. Refresh MCP discovery to load the added optional parameter.

## Lightweight password sign-in

Unauthenticated requests now receive a small sign-in document with inline critical styles and one small independent JavaScript file. The terminal, explorer, monitor and other application assets load only after authentication. The gateway serves the sign-in page and script locally, without waiting for runtime health, cold-start completion, or terminal assets. Ready-state app/auth requests no longer perform a redundant health-check round trip. Preview routes retain their readiness checks.

The form has keyboard submission, password reveal, mobile sizing, visible pending/error states, duplicate-submit prevention and a 12-second client request deadline. Password URL sign-in in browsers removes the credential from browser history before exchanging it by JSON POST. Existing non-browser password-URL redirects remain compatible. Authentication, password hashing, origin/CSRF checks, secure cookies and rate limits are not weakened.


The instance advertises `webterm` with one required string, `cmd`, plus native `get_image`. The Colab gateway advertises `colab`, `webterm` and `get_image`. Old workspace/terminal/lifecycle tool names remain callable only for compatibility; new discovery no longer includes them.

## Commands and real Bash pipelines

```json
{"cmd":"webterm new /home/dev/project/app"}
{"cmd":"webterm ls terminals"}
{"cmd":"webterm read 123 | grep -i error | sed -n '1,20p'"}
{"cmd":"webterm write 123 --enter -- 'printf hello'"}
{"cmd":"webterm run /home/dev/project/app -- 'cargo test --locked'"}
{"cmd":"webterm read 123 --json --wait 20"}
{"cmd":"webterm ls workspaces"}
{"cmd":"webterm help"}
```

Replace 123 with a returned **native terminal ID**. IDs are globally unique within the current WebTerm database. Read/write/resize/stop can infer the owning workspace from that ID, then check its canonical path against allowed roots. Supplying a workspace explicitly still enforces ownership. **Do not reuse the old gateway's 01/02 aliases as native IDs**; obtain current IDs with `webterm ls terminals`. Old wrapper calls keep their old mapping.

All MCP `cmd` execution goes through real Bash with `pipefail`. Pipes, redirections, variables, quoting, newlines and conditionals follow Bash rules. Native `read` emits raw retained text to stdout, so grep/sed/head see text rather than JSON, and filtering occurs before the final 2,000-character tool preview. Native `ls` emits tab-separated rows for piping. `--json` explicitly selects structured CLI output. A single unfiltered native command preserves its structured metadata in the MCP response.

Quote a complete script after `--`: `webterm run /path -- 'printf "%s\n" "a|b"'`. Likewise, quote input to write when it contains shell syntax. `write` sends Enter only with `--enter`. `webterm cmd 'OLD LITERAL GRAMMAR'` remains an explicit compatibility route for pre-Bash callers, including literal multiline payloads.

The compact CLI supports `new`, `read`, `write`, `run`, `python`, `ls`, `ensure`, `resize`, `stop`, `status` and `help`. `ls terminals` accepts an optional workspace; `ls workspaces` lists folders. Listings default to 50 rows, accept `--limit 1..200` and `--offset`, and include `next_offset` only when another page exists. Use `--json` to retain pagination metadata in direct CLI output. Existing nested workspace/terminal CLI commands remain available.

## Persistent work versus bounded controls

Native control commands execute in a separate Bash wrapper and consume **zero persistent PTY slots**. Read/list/stop still work when the runtime's persistent terminal capacity is full. The control wrapper has a 25-second deadline, a 16 MiB I/O ceiling, bounded retained output, and process-group cleanup. It is for controls and filters, not detached background servers.

Use `webterm run /workspace -- 'LONG SCRIPT'` or `webterm python /workspace -- 'PYTHON CODE'` for persistent work. `--wait 0..20` defaults to 20; longer work returns `running:true` and a terminal ID. Read that ID instead of rerunning. A completed run/python CLI propagates its command's exit status into Bash conditionals; read itself reports read/pipeline success separately from the source command status.

Ordinary Bash scripts such as `cd /home/dev/project/app && npm test` also run through the persistent command runner. They wait up to 20 seconds and keep running with a returned handle. Their command-only wrapper shells close automatically after completion; retained output remains readable. Explicitly created persistent shells are never automatically stopped. The default workspace for ordinary Bash is the first configured workspace root; use `webterm run` to select one explicitly.

There are at most eight simultaneous control calls. A busy response starts no command. Control timeouts or a failed shell sequence may follow earlier successful side effects: do not blindly resubmit. For several slow native operations, use nonblocking `run --wait 0` and collect their IDs.

## Output, filtering and polling

Default MCP output over 2,000 Unicode characters returns its first 500 and last 1,500. `chars` and `omitted` disclose truncation. `--max-chars` and `--full` are explicit native-read overrides. Full means retained history, not unlimited output: command retention remains bounded at 262,144 characters, and ordinary screen capture has its separate byte/line limits. Save unlimited logs explicitly in workspace files.

Read responses include a stateless snapshot fingerprint. `webterm read ID --if-changed SNAPSHOT --wait 20` omits unchanged output and waits for output/completion changes. Use the actual returned fingerprint. It is not a cryptographic token or a resumable log cursor.

A filtered result exposes the Bash pipeline's `exit_code`; `source_terminal_id` and `source_exit_code` identify the source when one native read was used. For example, grep returning no matches has exit code 1 even when the source command succeeded. Source retention/capture limitations are propagated when applicable. Early-closing head/grep filters do not fail merely because they intentionally close the native read stream.

The previous `read --filter 'BASH'` flag is retained; its five-second filter limit and separate status remain unchanged. Normal pipelines are preferred. Optional `--task NAME --summary '35/100 Brief action'` is still supported by native operations. The legacy literal command route preserves audit attribution.

## Colab management and proxy

`colab(cmd)` accepts `status`, `start [highram|l4|tpu-v5e1|tpu-v6e1]`, `backup`, `details [timing|backup|graphics|errors]`, `stop` and `help`. It cannot select another Colab session. **Stop never backs up**. Verify backup success before stopping when persistence matters.

The gateway forwards WebTerm commands, native IDs, schemas, structured responses, errors and image blocks without alias rewriting. Discovery has a 15-second cache, a two-second upstream discovery deadline, a private last-known catalog and an offline fallback. Discovery does not allocate or start a VM. Restart/reconnect the MCP client to refresh its discovered tool catalog.

Authentication, allowed-root checks, origin checks and native image limits are unchanged. Bash has the operating-system user's permissions; this is not a filesystem or network sandbox. Keep secrets out of command text and tool logs. No public tunnel is opened automatically. Private application previews keep their existing per-port origins.

## Tests

Run `cargo test --locked`, `cargo build --locked --all-targets`, and `python3 tests/unified_webterm_integration.py`. The integration suite creates an isolated database, PTY runtime and loopback servers, validates Bash pipelines and terminal-capacity behavior, exercises actual Chrome desktop/mobile views, and cleans up only its own fixtures. Set `WEBTERM_TEST_BIN` and `WEBTERM_TEST_REPORT` to override paths.


## Explicit per-call workspace and progress metadata

`webterm` accepts optional `workspace`, `task`, and `summary` alongside `cmd` and `text`. Include all three on read/write calls, including Bash read pipelines. `workspace` is an absolute existing folder within configured roots; it selects Bash cwd and validates terminal ownership. A conflicting inline workspace is rejected.

`task` is a simple single-line name (1–80 characters). Supply `task` and `summary` together. `summary` starts with `0/100` through `100/100`, followed by a description of this call and its current progress. It must have fewer than 50 words in total, no control characters, and at most 2,048 characters.

```json
{"cmd":"webterm read 12 | grep error","workspace":"/home/dev/project/app","task":"Build checks","summary":"65/100 Reading test output; build completed, checking remaining errors"}
```

For write, use `cmd: "webterm write 12 --enter"` and put literal terminal input in `text`. Explicit attribution wins over legacy inline `--task`/`--summary`; metadata is never evaluated as shell source. Concurrent calls carry independent child-process contexts. Both native WebTerm and the Colab gateway log the fields. Calls without the new fields remain compatible.

Workspace sidebar: single click or Enter/Space folds/unfolds only. Rapid double-click/tap activates the workspace. Three nearby clicks/taps also activate it. The third click after a double-click cannot collapse it again.

## Workspace-only commands and task continuity

Use `webterm({"cmd":"webterm run","workspace":"/home/dev/project/app","text":"pwd","task":"App build verification","summary":"40/100 Progress: parser fixed; verifying the build"})`.
The workspace parameter sets the working directory for run/python and ordinary Bash, and scopes reads/writes. Do not repeat the path in cmd. Legacy positional paths still work; a conflicting path is rejected before execution.

Choose a descriptive task name when work starts and reuse that exact name across related calls and follow-up chats. Avoid generic names such as `webterm`, `run`, or `task`. Summaries must start with honest current progress or quality `n/100`, give concrete current status, and contain fewer than 50 words. Do not reset progress to zero on each call or claim completion before testing.

Modern Webterm MCP results contain terminal output in `structuredContent.text`, with `content: []` instead of a duplicate JSON text block. Error text and native image blocks are retained. Hidden legacy tools and the internal CLI/PTY protocol keep their existing keys. Clients must consume structuredContent; old clients that read only content will need updating. Log details also collapse duplicate payloads in historical entries without rewriting stored history.


### Short output and error diagnostics (v0.2.2)

Compact command text is limited by default to 1,000 Unicode characters: exactly the first 200 and last 800 when longer. The middle is omitted without rerunning the command. The result includes `omitted` and `read_more`, for example `webterm read 42 --full`. Run/python responses remain capped even with `--full`; use an explicit read to expand retained output, or `read ID --max-chars N` for a chosen budget. The retention limit still applies.

Standard error is included in the returned `text`, not hidden in a separate field. Run commands retain a separate bounded stderr diagnostic copy, so an early error is not lost behind a noisy stdout tail. Both streams still behave as terminals. When diagnostics would otherwise be omitted, they are appended before applying the same preview budget. Filter/control/helper failures include their stderr too. A write reports only a compact receipt; use `webterm read ID` to see the command result, including stderr. The write input is never echoed in the receipt.

Piped controls retain no new terminal: when `source_terminal_id` is available, the read-more hint explicitly refers to the original unfiltered terminal. Controls without an ID do not invent one. Structured lists remain paginated metadata; the preview applies to textual output and error messages. Shortening does not change command exit status.
