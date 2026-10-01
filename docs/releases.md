# Building and publishing portable releases

Linux x86_64 releases target `x86_64-unknown-linux-musl`. The package script
rejects an ELF interpreter or `DT_NEEDED` entries, checks the executable version,
and records the exact Git commit, Rust compiler and SHA-256 in BUILD-INFO.json.
Bash and Python 3 are runtime requirements for shell/MCP execution, not bundled
libraries. A Rust compiler is not needed to run the downloaded executable.

Build prerequisites: a Rust toolchain supporting the package, the musl target,
`musl-gcc` (Debian/Ubuntu: `musl-tools`), Python 3.11+, Git, binutils, tar and gzip.

```sh
rustup target add x86_64-unknown-linux-musl
cargo test --locked
node tests/command_log_format.cjs
# Commit the intended source first: release packaging requires a clean checkout.
deploy/build-release.sh /absolute/output/directory
```

Use CARGO_TARGET_DIR to keep compiler intermediates outside persistent source
storage. A relocated musl installation may supply
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER and
CC_x86_64_unknown_linux_musl explicitly. These must point to a working toolchain;
the release script does not modify host configuration or install dependencies.

The output contains the compatibility-named executable `webterm-linux-x86_64`,
a versioned tarball with install/configuration guidance, BUILD-INFO.json,
SHA256SUMS, and release notes. Verify `sha256sum -c SHA256SUMS` before installation.
The source commit time is used for tar entry timestamps, but different compilers
or build environments can still produce different executable hashes.

Bump Cargo.toml and the root package in Cargo.lock, add a CHANGELOG.md section,
validate the exact release binary, commit, and push the source. Publish the
verified assets manually with `gh release create vX.Y.Z --target COMMIT`, passing
the executable, tarball, BUILD-INFO.json and SHA256SUMS as asset arguments. The
release tag must match the package version and the source commit recorded in
BUILD-INFO.json. Do not overwrite assets from an existing published release.

The repository's existing tag-push workflow is a legacy GNU-target build and
does not use this static packaging helper. Updating that workflow requires
GitHub workflow-write permission; it is intentionally unchanged in v0.2.2.
Do not rely on that legacy workflow for portable musl release artifacts.

Full local regression coverage, including real PTYs, Chrome and proxies:

```sh
WEBTERM_TEST_BIN=/absolute/path/to/webterm-linux-x86_64 \
WEBTERM_TEST_REPORT=/absolute/evidence/integration.json \
python3 tests/unified_webterm_integration.py
```

This suite additionally requires Google Chrome, websocket-client for Python,
and writable temporary/build paths. It starts isolated services and never
reuses the production database or personal browser profile.


### Short output and error diagnostics (v0.2.2)

Compact command text is limited by default to 1,000 Unicode characters: exactly the first 200 and last 800 when longer. The middle is omitted without rerunning the command. The result includes `omitted` and `read_more`, for example `webterm read 42 --full`. Run/python responses remain capped even with `--full`; use an explicit read to expand retained output, or `read ID --max-chars N` for a chosen budget. The retention limit still applies.

Standard error is included in the returned `text`, not hidden in a separate field. Run commands retain a separate bounded stderr diagnostic copy, so an early error is not lost behind a noisy stdout tail. Both streams still behave as terminals. When diagnostics would otherwise be omitted, they are appended before applying the same preview budget. Filter/control/helper failures include their stderr too. A write reports only a compact receipt; use `webterm read ID` to see the command result, including stderr. The write input is never echoed in the receipt.

Piped controls retain no new terminal: when `source_terminal_id` is available, the read-more hint explicitly refers to the original unfiltered terminal. Controls without an ID do not invent one. Structured lists remain paginated metadata; the preview applies to textual output and error messages. Shortening does not change command exit status.
