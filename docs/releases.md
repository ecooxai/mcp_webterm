# Building and publishing Linux releases

Linux x86_64 releases target `x86_64-unknown-linux-gnu` and use the system **glibc** ABI. The package helper verifies an ELF interpreter and `libc.so.6`, then records the required GLIBC symbol version, shared libraries, exact Git commit, compiler, and SHA-256 in BUILD-INFO.json. A Rust compiler is not needed on the target host. Bash and Python 3 remain runtime requirements.

Build prerequisites: a standard Rust toolchain, GNU C/linker toolchain, Python 3.11+, Git, binutils, tar, gzip, and sha256sum. No musl toolchain is required.

```sh
cargo test --locked
node tests/command_log_format.cjs
# Commit the intended source first: release packaging requires a clean checkout.
CARGO_TARGET_DIR=/build/cargo-target deploy/build-release.sh /absolute/output/directory
```

The output contains `webterm-linux-x86_64`, a versioned tarball, BUILD-INFO.json, SHA256SUMS, and release notes. Verify `sha256sum -c SHA256SUMS` before installation. Check BUILD-INFO.json before deploying to an older distribution: `glibc_required_symbol_version` records the newest GLIBC symbol required by the binary, and `needed_libraries` records dynamic dependencies.

Bump Cargo.toml and the root package in Cargo.lock, add a CHANGELOG.md section, validate the exact release binary, commit, and push the source. Publish verified assets with `gh release create vX.Y.Z --target COMMIT`, passing the executable, tarball, BUILD-INFO.json and SHA256SUMS. The release tag must match the package version and the source commit recorded in BUILD-INFO.json. Do not overwrite assets from an existing published release.

The existing tag workflow also uses Rust's default GNU/glibc target. The manual helper adds the tarball and build metadata used for verified releases. Updating workflow files may require separate GitHub workflow-write permission.

Full local regression coverage, including real PTYs, Chrome and proxies:

```sh
WEBTERM_TEST_BIN=/absolute/path/to/webterm-linux-x86_64 \
WEBTERM_TEST_REPORT=/absolute/evidence/integration.json \
python3 tests/unified_webterm_integration.py
```

The suite additionally requires Google Chrome, websocket-client for Python, and writable temporary/build paths. It starts isolated services and never reuses the production database or personal browser profile.


### Short output and error diagnostics (v0.2.2)

Compact command text is limited by default to 1,000 Unicode characters: exactly the first 200 and last 800 when longer. The middle is omitted without rerunning the command. The result includes `omitted` and `read_more`, for example `webterm read 42 --full`. Run/python responses remain capped even with `--full`; use an explicit read to expand retained output, or `read ID --max-chars N` for a chosen budget. The retention limit still applies.

Standard error is included in the returned `text`, not hidden in a separate field. Run commands retain a separate bounded stderr diagnostic copy, so an early error is not lost behind a noisy stdout tail. Both streams still behave as terminals. When diagnostics would otherwise be omitted, they are appended before applying the same preview budget. Filter/control/helper failures include their stderr too. A write reports only a compact receipt; use `webterm read ID` to see the command result, including stderr. The write input is never echoed in the receipt.

Piped controls retain no new terminal: when `source_terminal_id` is available, the read-more hint explicitly refers to the original unfiltered terminal. Controls without an ID do not invent one. Structured lists remain paginated metadata; the preview applies to textual output and error messages. Shortening does not change command exit status.
