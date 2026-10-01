# Changelog

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
