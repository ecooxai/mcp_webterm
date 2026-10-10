#!/usr/bin/env python3
"""Stress the native PTY runtime with thousands of terminals.

Usage: tests/runtime_stress.py --binary target/release/webterm [--count 2000]

Starts an isolated runtime (own socket, state dir, swap dir, short idle
timeout), creates N terminals, keeps a slice of them busy, then lets the
rest go idle and checks they hibernate to disk and resume. Prints CPU and
RSS samples and exits non-zero when a budget is exceeded.
"""
import argparse, json, os, shutil, signal, socket, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor

CLK = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")


def rpc(sock_path, request, timeout=30):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(sock_path)
        s.sendall(json.dumps(request).encode() + b"\n")
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    response = json.loads(buf)
    if not response.get("ok"):
        raise RuntimeError(f"{request.get('op')}: {response.get('error')}")
    return response.get("result")


def tree_pids(root):
    pids, children = {root}, {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(ppid, []).append(int(entry))
    stack = [root]
    while stack:
        for child in children.get(stack.pop(), []):
            if child not in pids:
                pids.add(child)
                stack.append(child)
    return pids


def swap_files(swap):
    found = []
    for base, _, files in os.walk(swap):
        found += [os.path.join(base, f) for f in files if f.endswith(".screen")]
    return found


def usage(pids):
    """(cpu_ticks, pss_bytes, threads) summed over pids.

    PSS splits shared pages (bash text, libc) between processes, so the
    total is real memory use rather than N copies of shared libraries.
    """
    ticks = rss = threads = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as f:
                fields = f.read().rsplit(")", 1)[1].split()
            ticks += int(fields[11]) + int(fields[12])
            threads += int(fields[17])
            with open(f"/proc/{pid}/smaps_rollup") as f:
                for line in f:
                    if line.startswith("Pss:"):
                        rss += int(line.split()[1]) * 1024
                        break
        except (OSError, IndexError, ValueError):
            pass
    return ticks, rss, threads


def sample(label, root, seconds, only_root=False):
    pids = {root} if only_root else tree_pids(root)
    t0, _, _ = usage(pids)
    time.sleep(seconds)
    pids = {root} if only_root else tree_pids(root)
    t1, rss, threads = usage(pids)
    cpu = (t1 - t0) / CLK / seconds * 100
    print(f"  {label:<34} cpu={cpu:6.1f}%  pss={rss/2**20:8.1f} MiB  threads={threads}  procs={len(pids)}", flush=True)
    return cpu, rss, threads


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/webterm")
    ap.add_argument("--count", type=int, default=2000)
    ap.add_argument("--busy", type=int, default=100, help="terminals kept producing output")
    ap.add_argument("--idle-seconds", type=int, default=45)
    ap.add_argument("--max-idle-cpu", type=float, default=5.0, help="runtime %% CPU budget when all idle")
    ap.add_argument("--max-dormant-rss-mib", type=float, default=300.0)
    ap.add_argument("--full-swap", action="store_true",
                    help="point WEBTERM_SWAP_DIR at a full 1 MiB tmpfs (needs sudo) and expect fallback to $TMPDIR")
    args = ap.parse_args()

    root = tempfile.mkdtemp(prefix="webterm-stress-")
    sock = os.path.join(root, "runtime.sock")
    swap = os.path.join(root, "swap")
    preferred = swap
    tmpdir = os.path.join(root, "tmpdir")
    work = os.path.join(root, "work")
    os.makedirs(work)
    os.makedirs(tmpdir)
    full = None
    if args.full_swap:
        full = os.path.join(root, "full")
        os.makedirs(full)
        subprocess.run(["sudo", "-n", "mount", "-t", "tmpfs", "-o", f"size=1m,uid={os.getuid()},mode=0700",
                        "tmpfs", full], check=True)
        with open(os.path.join(full, "filler"), "wb") as f:
            try:
                f.write(b"\0" * (2 << 20))
            except OSError:
                pass
        preferred, swap = full, tmpdir
        print(f"preferred swap {full} is full; expecting fallback under {tmpdir}")
    env = dict(os.environ, XDG_STATE_HOME=os.path.join(root, "state"), HOME=root, TMPDIR=tmpdir,
               WEBTERM_RUNTIME_SOCKET=sock, WEBTERM_SWAP_DIR=preferred,
               WEBTERM_IDLE_SECONDS=str(args.idle_seconds), SHELL="/bin/bash",
               RUST_LOG="warn")
    env.pop("WEBTERM_CONFIG", None)
    log = open(os.path.join(root, "runtime.log"), "w")
    proc = subprocess.Popen([args.binary, "runtime"], env=env, stdout=log, stderr=log,
                            start_new_session=True)
    failures = []
    try:
        for _ in range(100):
            if os.path.exists(sock):
                break
            time.sleep(0.1)
        ids = [f"pty-stress-{i:05d}" for i in range(args.count)]
        print(f"runtime pid={proc.pid} root={root}")
        sample("empty runtime", proc.pid, 2)

        start = time.time()
        with ThreadPoolExecutor(32) as pool:
            list(pool.map(lambda i: rpc(sock, {"op": "create", "id": i, "cwd": work}), ids))
        print(f"created {args.count} terminals in {time.time()-start:.1f}s")
        stats = rpc(sock, {"op": "stats"})
        print("  stats:", stats)
        if stats["sessions"] != args.count or stats["active_shells"] != args.count:
            failures.append(f"expected {args.count} active sessions, got {stats}")

        time.sleep(3)
        cpu, _, _ = sample(f"{args.count} idle shells (runtime only)", proc.pid, 5, only_root=True)
        sample(f"{args.count} idle shells (whole tree)", proc.pid, 5)
        if cpu > args.max_idle_cpu:
            failures.append(f"idle runtime CPU {cpu:.1f}% > {args.max_idle_cpu}%")

        busy = ids[: args.busy]
        cmd = "for i in $(seq 1 600); do echo line-$i-$RANDOM; sleep 0.05; done\r"
        with ThreadPoolExecutor(32) as pool:
            list(pool.map(lambda i: rpc(sock, {"op": "write", "id": i, "data": list(cmd.encode())}), busy))
        sample(f"{args.busy} busy + rest idle (runtime)", proc.pid, 5, only_root=True)
        text = rpc(sock, {"op": "capture", "id": busy[0], "lines": 50})
        if "line-" not in text:
            failures.append("busy terminal produced no output")

        start = time.time()
        listed = rpc(sock, {"op": "list"})
        print(f"list RPC: {len(listed)} ids in {(time.time()-start)*1000:.1f} ms")

        deadline = time.time() + args.idle_seconds + 90
        while time.time() < deadline:
            stats = rpc(sock, {"op": "stats"})
            if stats["dormant"] >= args.count - args.busy and stats["swapped_screens"] >= args.count - args.busy:
                break
            time.sleep(2)
        print("  after idle timeout:", stats)
        if stats["dormant"] < args.count - args.busy:
            failures.append(f"idle terminals not hibernated: {stats}")
        if stats["sessions"] != args.count:
            failures.append(f"hibernation lost sessions: {stats}")
        files = swap_files(swap)
        print(f"  swap files: {len(files)}  bytes={sum(os.path.getsize(f) for f in files)}")
        if len(files) < args.count - args.busy:
            failures.append(f"only {len(files)} swap files")
        bad = [f for f in files if os.stat(f).st_mode & 0o077]
        if bad:
            failures.append(f"swap files readable by others: {bad[:3]}")
        sample(f"{args.busy} busy + rest dormant (runtime)", proc.pid, 5, only_root=True)
        sample(f"{args.busy} busy + rest dormant (tree)", proc.pid, 3)

        # Busy terminals finish their loop (~30s) and then hibernate too.
        deadline = time.time() + 30 + args.idle_seconds + 60
        while time.time() < deadline:
            stats = rpc(sock, {"op": "stats"})
            if stats["dormant"] == args.count:
                break
            time.sleep(2)
        print("  all idle:", stats)
        if stats["dormant"] != args.count:
            failures.append(f"busy terminals never hibernated: {stats}")
        cpu, rss, _ = sample(f"{args.count} dormant (runtime only)", proc.pid, 10, only_root=True)
        sample(f"{args.count} dormant (whole tree)", proc.pid, 3)
        if cpu > args.max_idle_cpu:
            failures.append(f"dormant runtime CPU {cpu:.1f}% > {args.max_idle_cpu}%")
        if rss / 2**20 > args.max_dormant_rss_mib:
            failures.append(f"dormant runtime PSS {rss/2**20:.0f} MiB > {args.max_dormant_rss_mib}")

        probe = ids[-1]
        before = rpc(sock, {"op": "info", "id": probe})
        if not before.get("hibernated"):
            failures.append(f"{probe} not hibernated: {before}")
        rpc(sock, {"op": "write", "id": probe, "data": list(b"echo RESUMED-$((40+2))\r")})
        ok = False
        for _ in range(50):
            if "RESUMED-42" in rpc(sock, {"op": "capture", "id": probe, "lines": 50}):
                ok = True
                break
            time.sleep(0.1)
        after = rpc(sock, {"op": "info", "id": probe})
        print(f"resume {probe}: ok={ok} info={after}")
        if not ok or not after.get("running") or after.get("hibernated"):
            failures.append(f"resume failed: {after}")

        start = time.time()
        with ThreadPoolExecutor(32) as pool:
            list(pool.map(lambda i: rpc(sock, {"op": "stop", "id": i}), ids))
        print(f"stopped {args.count} terminals in {time.time()-start:.1f}s")
        stats = rpc(sock, {"op": "stats"})
        left = swap_files(swap)
        print("  final:", stats, "swap files left:", len(left))
        if stats["sessions"] or left:
            failures.append(f"cleanup incomplete: {stats}, {len(left)} swap files")
    finally:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
        if full:
            if swap_files(full):
                failures.append("swap file written to the full folder")
            subprocess.run(["sudo", "-n", "umount", full])
        if not failures:
            shutil.rmtree(root, ignore_errors=True)
    if failures:
        print("FAIL:\n  " + "\n  ".join(failures))
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
