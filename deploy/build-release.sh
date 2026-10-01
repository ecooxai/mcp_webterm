#!/usr/bin/env bash
# Build a clean, identified Linux x86_64 release; never publish or alter services.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$root"
for tool in cargo rustc python3 git readelf install tar gzip sha256sum; do
  command -v "$tool" >/dev/null || { printf 'Missing build dependency: %s\n' "$tool" >&2; exit 1; }
done
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Expected a stable semantic version' >&2; exit 1; }
if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  echo 'Release builds require a clean working tree. Commit the intended source changes first.' >&2
  exit 1
fi
commit=$(git rev-parse HEAD)
epoch=$(git show -s --format=%ct HEAD)
target=x86_64-unknown-linux-gnu
output=${1:-"$root/dist/release-v$version"}
mkdir -p -- "$output"
output=$(cd -- "$output" && pwd)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$root/target"}
cargo build --release --locked --target "$target"
binary="$CARGO_TARGET_DIR/$target/release/webterm"
interpreter=$(readelf -l "$binary" | sed -n 's/.*Requesting program interpreter: \([^]]*\)\].*/\1/p' | head -n 1)
needed=$(readelf -d "$binary" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p')
if [[ -z "$interpreter" || "$needed" != *libc.so.6* ]]; then
  echo 'Refusing a GNU release that is not dynamically linked against glibc libc.so.6.' >&2
  exit 1
fi
glibc_required=$(readelf --version-info "$binary" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)*' | sort -Vu | tail -n 1 || true)
needed_csv=$(printf '%s\n' "$needed" | paste -sd, -)
[[ "$("$binary" --version)" == "webterm $version" ]] || { echo 'Binary version does not match Cargo.toml' >&2; exit 1; }
install -m 0755 -- "$binary" "$output/webterm-linux-x86_64"
python3 - "$output" "$version" "$commit" "$target" "$epoch" "$interpreter" "$glibc_required" "$needed_csv" <<'PY'
import hashlib,json,subprocess,sys
from pathlib import Path
out=Path(sys.argv[1]);version,commit,target,epoch,interpreter,glibc_required,needed_csv=sys.argv[2:]
binary=out/'webterm-linux-x86_64'
with binary.open('rb') as f:sha=hashlib.file_digest(f,'sha256').hexdigest()
info={'project':'webterm','version':version,'git_commit':commit,'target':target,
      'source_commit_time_unix':int(epoch),'source_dirty':False,'linkage':'dynamic glibc',
      'elf_interpreter':interpreter,'glibc_required_symbol_version':glibc_required or None,
      'needed_libraries':[x for x in needed_csv.split(',') if x],
      'rustc':subprocess.check_output(['rustc','--version'],text=True).strip(),
      'binary_sha256':sha,'binary_bytes':binary.stat().st_size,
      'runtime_requirements':['Linux x86_64', ('glibc >= '+glibc_required.removeprefix('GLIBC_')) if glibc_required else 'compatible glibc', 'Bash','Python 3 for shell/MCP operations']}
(out/'BUILD-INFO.json').write_text(json.dumps(info,indent=2)+'\n')
changelog=Path('CHANGELOG.md').read_text();heading='## '+version+' '
sections=changelog.split('\n## ')
notes=next((s for s in sections if s.startswith(version+' ')),None)
if notes is None:raise SystemExit('Missing release notes for '+version)
(out/'RELEASE-NOTES.md').write_text('## '+notes.strip()+'\n')
PY
stage=$(mktemp -d "${TMPDIR:-/tmp}/webterm-release.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
bundle="webterm-v$version-linux-x86_64"
mkdir -p -- "$stage/$bundle/deploy"
install -m 0755 -- "$binary" "$stage/$bundle/webterm"
install -m 0644 -- "$output/BUILD-INFO.json" "$output/RELEASE-NOTES.md" "$stage/$bundle/"
install -m 0644 -- deploy/webterm.toml.example deploy/webterm.service deploy/webterm-runtime.service "$stage/$bundle/deploy/"
cat > "$stage/$bundle/INSTALL.md" <<'INSTALL'
# WebTerm Linux x86_64

This executable uses the normal GNU/Linux ABI and is dynamically linked against
glibc. The target host must provide compatible glibc and the shared libraries
listed in BUILD-INFO.json. A Rust compiler is not required. Shell/MCP operations
still require Bash and Python 3; this bundle does not include those interpreters.

Verify the downloaded files with `sha256sum -c SHA256SUMS`, extract the archive,
and run `./webterm --version`. Install the executable in a directory on PATH.
Use `deploy/webterm.toml.example` as a configuration template; set private
workspace roots, state paths, browser credentials and a separate MCP token.
Never expose an unauthenticated command service to the public network.

The native PTY runtime and HTTP frontend are separate processes:

    webterm --config /path/to/webterm.toml runtime
    webterm --config /path/to/webterm.toml serve

The deploy directory contains optional systemd templates; adapt paths and the
service user before installation. Restarting only the frontend preserves PTYs.
Read RELEASE-NOTES.md for the structuredContent.text migration.
INSTALL
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  -C "$stage" -cf - "$bundle" | gzip -n -9 > "$output/$bundle.tar.gz"
(cd -- "$output" && sha256sum webterm-linux-x86_64 "$bundle.tar.gz" BUILD-INFO.json > SHA256SUMS && sha256sum -c SHA256SUMS)
printf 'Release artifacts: %s\n' "$output"
