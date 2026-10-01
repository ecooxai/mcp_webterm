## 0.2.1

- Publish Linux x86_64 releases as static musl binaries so they run on older glibc hosts.

# Changelog

## 0.2.0 - 2026-09-29

- Consolidate terminal/workspace MCP operations behind Bash-backed `webterm(cmd)` while keeping native image output.
- Add literal `text` payloads for `run`, `python`, and `write`, plus compact polling and terminal listing.
- Load a lightweight independent password screen before xterm and workspace assets.
- Make mobile terminal output long-press selectable with drag selection and Copy/Clear actions, even while application mouse reporting is enabled.
- Make workspace-name clicks expand/collapse and double-clicks open the workspace as active.
- Preserve persistent PTY sessions across frontend-only deployment and improve isolated browser/integration coverage.
