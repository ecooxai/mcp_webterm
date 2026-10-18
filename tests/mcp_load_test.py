#!/usr/bin/env python3
"""Boot an isolated webterm (runtime + serve), open N terminals through MCP and
report MCP latency plus CPU/RAM of the runtime, the server and the shells."""
import argparse, concurrent.futures as cf, json, os, random, secrets, signal, socket, statistics
import subprocess, sys, time, urllib.request
from pathlib import Path


def cpu_seconds(text):
    days, _, rest = text.strip().rpartition("-")
    total = 0.0
    for part in rest.split(":"):
        total = total * 60 + float(part)
    return total + (int(days) * 86400 if days else 0)


def process_table():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss=,time="], capture_output=True, text=True).stdout
    rows = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 4:
            rows[int(parts[0])] = (int(parts[1]), int(parts[2]), cpu_seconds(parts[3]))
    return rows


def descendants(rows, root):
    children = {}
    for pid, (ppid, _, _) in rows.items():
        children.setdefault(ppid, []).append(pid)
    found, stack = [], list(children.get(root, []))
    while stack:
        pid = stack.pop()
        found.append(pid)
        stack.extend(children.get(pid, []))
    return found


def snapshot(runtime, server):
    rows = process_table()
    shells = descendants(rows, runtime)
    pick = lambda pids: (sum(rows[p][1] for p in pids if p in rows), sum(rows[p][2] for p in pids if p in rows))
    return {"t": time.time(), "runtime": pick([runtime]), "server": pick([server]), "shells": pick(shells), "shell_count": len(shells)}


def usage(a, b):
    wall = b["t"] - a["t"]
    result = {"seconds": round(wall, 1), "shell_processes": b["shell_count"]}
    for key in ("runtime", "server", "shells"):
        result[key] = {"rss_mib": round(b[key][0] / 1024, 1), "cpu_percent": round((b[key][1] - a[key][1]) / wall * 100, 2)}
    return result


def sample(label, runtime, server, seconds, report):
    start = snapshot(runtime, server)
    time.sleep(seconds)
    report["phases"][label] = usage(start, snapshot(runtime, server))
    print(label, json.dumps(report["phases"][label]), flush=True)


def latency(values):
    if not values:
        return {}
    ordered = sorted(values)
    at = lambda q: round(ordered[min(len(ordered) - 1, int(q * len(ordered)))] * 1000, 1)
    return {"count": len(values), "p50_ms": at(.5), "p95_ms": at(.95), "p99_ms": at(.99), "max_ms": round(ordered[-1] * 1000, 1), "mean_ms": round(statistics.mean(values) * 1000, 1)}


def find_terminal_id(value):
    if isinstance(value, dict):
        for key in ("terminal_id", "id"):
            if isinstance(value.get(key), int):
                return value[key]
        for child in value.values():
            found = find_terminal_id(child)
            if found is not None:
                return found
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/webterm")
    ap.add_argument("--root", default=str(Path.home() / "wt-load"))
    ap.add_argument("--port", type=int, default=17690)
    ap.add_argument("--count", type=int, default=1000)
    ap.add_argument("--concurrency", type=int, default=32)
    ap.add_argument("--busy", type=int, default=50)
    ap.add_argument("--idle-seconds", type=int, default=30)
    ap.add_argument("--report", default="")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    subprocess.run(["rm", "-rf", str(root)], check=True)
    (root / "state").mkdir(parents=True)
    (root / "proj").mkdir()
    config = root / "webterm.toml"
    config.write_text(
        f'listen = "127.0.0.1:{args.port}"\ndatabase_path = "{root}/state/webterm.db"\n'
        f'runtime_socket = "{root}/state/runtime.sock"\ntmux_socket = "{root}/state/tmux.sock"\n'
        f'workspace_roots = ["/"]\nautoboot_enabled = false\n'
    )
    token = secrets.token_hex(24)
    env = {**os.environ, "WEBTERM_AUTH_TOKEN": token, "WEBTERM_PROXY_LISTEN": "127.0.0.1:0"}
    binary = str(Path(args.binary).resolve())
    logs = [open(root / "runtime.log", "w"), open(root / "serve.log", "w")]
    runtime = subprocess.Popen([binary, "--config", str(config), "runtime"], env=env, stdout=logs[0], stderr=subprocess.STDOUT, start_new_session=True)
    while not (root / "state/runtime.sock").exists():
        time.sleep(.1)
    server = subprocess.Popen([binary, "--config", str(config), "--passwd", secrets.token_hex(8), "serve"], env=env, stdout=logs[1], stderr=subprocess.STDOUT, start_new_session=True)
    for _ in range(200):
        try:
            socket.create_connection(("127.0.0.1", args.port), .2).close()
            break
        except OSError:
            time.sleep(.1)

    workspace = str(root / "proj")
    url = f"http://127.0.0.1:{args.port}/mcp?output_type=json"

    def mcp(cmd):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "webterm", "arguments": {
            "cmd": cmd, "workspace_id": workspace, "task": "load test", "summary": "50/100 load test"}}}).encode()
        req = urllib.request.Request(url, data=body, headers={"Authorization": "Bearer " + token, "content-type": "application/json"})
        started = time.perf_counter()
        with urllib.request.urlopen(req, timeout=120) as response:
            result = json.load(response).get("result", {})
        elapsed = time.perf_counter() - started
        if result.get("isError"):
            raise RuntimeError(result.get("content", [{}])[0].get("text", "error")[:200])
        return result.get("structuredContent") or {}, elapsed

    def batch(label, commands):
        times, results, errors = [], [None] * len(commands), []
        started = time.perf_counter()
        with cf.ThreadPoolExecutor(args.concurrency) as pool:
            futures = {pool.submit(mcp, cmd): i for i, cmd in enumerate(commands)}
            for future in cf.as_completed(futures):
                try:
                    results[futures[future]], elapsed = future.result()
                    times.append(elapsed)
                except Exception as error:  # noqa: BLE001 - report every failure
                    errors.append(str(error))
        wall = time.perf_counter() - started
        report["mcp"][label] = {**latency(times), "errors": len(errors), "sample_errors": sorted(set(errors))[:5],
                                "wall_s": round(wall, 2), "calls_per_s": round(len(commands) / wall, 1)}
        print(label, json.dumps(report["mcp"][label]), flush=True)
        return results

    report = {"platform": sys.platform, "count": args.count, "concurrency": args.concurrency, "phases": {}, "mcp": {}}
    try:
        sample("baseline", runtime.pid, server.pid, 3, report)
        before = snapshot(runtime.pid, server.pid)
        created = batch("create_terminals", [f"new load{i:04d}" for i in range(args.count)])
        report["phases"]["during_create"] = usage(before, snapshot(runtime.pid, server.pid))
        ids = [find_terminal_id(item) for item in created if item]
        ids = [i for i in ids if i is not None]
        report["terminals_created"] = len(ids)
        print("created", len(ids), flush=True)
        if not ids:
            raise SystemExit("no terminals created; see serve.log/runtime.log in --root")
        sample("settled_after_create", runtime.pid, server.pid, 10, report)

        before = snapshot(runtime.pid, server.pid)
        batch("write_each", [f"write {tid} --enter -- echo ready-{tid}" for tid in ids])
        reads = batch("read_each", [f"read {tid}" for tid in ids])
        report["phases"]["during_write_read"] = usage(before, snapshot(runtime.pid, server.pid))
        report["echo_verified"] = sum(1 for tid, item in zip(ids, reads) if item and f"ready-{tid}" in json.dumps(item))
        print("echo verified", report["echo_verified"], "of", len(ids), flush=True)
        report["sample_read"] = reads[0] if reads else None
        if not report["echo_verified"]:
            print("sample read:", json.dumps(report["sample_read"])[:600], flush=True)

        sample("idle_all_terminals", runtime.pid, server.pid, args.idle_seconds, report)

        busy = ids[: args.busy]
        batch("start_busy", [f"write {tid} --enter -- while :; do date; sleep 0.05; done" for tid in busy])
        before = snapshot(runtime.pid, server.pid)
        probe = [random.choice(["status", "ls terminals --limit 50"] + [f"read {random.choice(ids)} --lines 20" for _ in range(4)]) for _ in range(400)]
        batch("mcp_under_load", probe)
        time.sleep(max(0, 15 - (time.time() - before["t"])))
        report["phases"]["busy_output"] = usage(before, snapshot(runtime.pid, server.pid))
        print("busy_output", json.dumps(report["phases"]["busy_output"]), flush=True)
        batch("stop_busy", [f"write {tid} -- \x03" for tid in busy])
        status, _ = mcp("status")
        report["status"] = status
        sample("idle_after_busy", runtime.pid, server.pid, 10, report)
        report["runtime_alive"] = runtime.poll() is None
        report["server_alive"] = server.poll() is None
    finally:
        for proc in (server, runtime):
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except OSError:
                pass
        time.sleep(2)
        for proc in (server, runtime):
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except OSError:
                pass
        if args.report:
            Path(args.report).write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
