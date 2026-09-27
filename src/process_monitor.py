"""Bounded Linux task-manager collector. No shell commands or environment disclosure."""
from __future__ import annotations
import csv
import glob
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import signal
import subprocess
import sys
import time

PROC = Path('/proc')
SYS = Path('/sys')
MAX_PROCESSES = 4096
MAX_ROWS = 2000
MAX_FDS = 512
MAX_FILE = 2 * 1024 * 1024
CLOCK = os.sysconf('SC_CLK_TCK')
PAGES = os.sysconf('SC_PAGE_SIZE')


def read(path, limit=65536):
    with Path(path).open('rb') as stream:
        return stream.read(limit).decode('utf-8', 'replace')


def safe_read(path, limit=65536):
    try:
        return read(path, limit)
    except (OSError, ValueError):
        return ''


def readlink(path):
    try:
        return os.readlink(path)
    except OSError:
        return ''


def stat_fields(text):
    end = text.rfind(')')
    begin = text.find('(')
    if begin < 0 or end < begin:
        raise ValueError('Malformed process stat')
    fields = text[end + 2:].split()
    if len(fields) < 22:
        raise ValueError('Incomplete process stat')
    return {'name': text[begin+1:end], 'state': fields[0], 'ppid': int(fields[1]),
            'ticks': int(fields[11]) + int(fields[12]), 'nice': int(fields[16]),
            'threads': int(fields[17]), 'start_time': str(int(fields[19])),
            'memory_bytes': max(0, int(fields[21])) * PAGES}


def key_values(text):
    result = {}
    for line in text.splitlines():
        key, sep, value = line.partition(':')
        if sep:
            result[key] = value.strip()
    return result


def number(value):
    try:
        return float(value)
    except (ValueError, TypeError):
        return None


def delta(current, previous, elapsed):
    if current is None or previous is None or elapsed <= 0 or current < previous:
        return None
    return (current - previous) / elapsed


def cpu_counters(text):
    parts = text.splitlines()[0].split()
    if parts[0] != 'cpu':
        raise ValueError('No CPU sample')
    values = [int(v) for v in parts[1:9]]
    return [sum(values), values[3] + (values[4] if len(values) > 4 else 0)]


def socket_address(raw):
    address, port = raw.split(':')
    data = bytes.fromhex(address)
    data = b''.join(data[i:i+4][::-1] for i in range(0, len(data), 4))
    return str(ipaddress.ip_address(data)), int(port, 16)


def ports_table(root=PROC):
    result = {}
    for protocol in ('tcp', 'tcp6', 'udp', 'udp6'):
        for line in safe_read(root/'net'/protocol, MAX_FILE).splitlines()[1:]:
            fields = line.split()
            try:
                if len(fields) < 10 or fields[3] not in (('0A',) if protocol.startswith('tcp') else ('07',)):
                    continue
                address, port = socket_address(fields[1])
                if port:
                    result[fields[9]] = {'port': port, 'address': address,
                        'protocol': 'tcp' if protocol.startswith('tcp') else 'udp',
                        'proxy_url': '/proxy/' + str(port) + '/' if protocol.startswith('tcp') else None}
            except (ValueError, IndexError):
                continue
    return result


def drm_fdinfo(text):
    fields = key_values(text)
    client = fields.get('drm-client-id')
    if not client:
        return None
    engine_ns = 0
    memory = 0
    found = False
    for key, value in fields.items():
        parts = value.split()
        if not parts:
            continue
        try:
            if key.startswith('drm-engine-') and len(parts) > 1 and parts[1] == 'ns':
                engine_ns += int(parts[0]); found = True
            elif key in ('drm-memory-vram', 'drm-total-vram', 'drm-memory-local0'):
                memory = max(memory, int(parts[0]) * (1024 if len(parts) > 1 and parts[1] in ('KiB', 'kB') else 1))
        except ValueError:
            continue
    return {'client': fields.get('drm-pdev', '') + ':' + client,
            'engine_ns': engine_ns if found else None, 'memory_bytes': memory or None}


def process_fds(base, ports):
    process_ports = {}
    gpu_clients = {}
    try:
        with os.scandir(base/'fd') as entries:
            for count, entry in enumerate(entries):
                if count >= MAX_FDS:
                    break
                target = readlink(entry.path)
                if target.startswith('socket:['):
                    item = ports.get(target[8:-1])
                    if item:
                        process_ports[(item['protocol'], item['address'], item['port'])] = item
                elif target.startswith('/dev/dri/'):
                    parsed = drm_fdinfo(safe_read(base/'fdinfo'/entry.name, 16384))
                    if parsed:
                        gpu_clients[parsed['client']] = parsed
    except OSError:
        pass
    engines = [c['engine_ns'] for c in gpu_clients.values() if c['engine_ns'] is not None]
    memory = [c['memory_bytes'] for c in gpu_clients.values() if c['memory_bytes'] is not None]
    return (sorted(process_ports.values(), key=lambda x: (x['port'], x['protocol'])),
            sum(engines) if engines else None, sum(memory) if memory else None)


def protected_pids(parent):
    protected = {1, os.getpid()}
    cursor = parent
    for _ in range(64):
        if cursor in protected or cursor < 1:
            break
        protected.add(cursor)
        try:
            cursor = stat_fields(read(PROC/str(cursor)/'stat'))['ppid']
        except (OSError, ValueError):
            break
    return protected


def username(uid):
    try:
        return pwd.getpwuid(uid).pw_name
    except KeyError:
        return str(uid)


def process_info(pid, protected, ports=None):
    base = PROC/str(pid)
    info = stat_fields(read(base/'stat', 8192))
    status = key_values(read(base/'status', 32768))
    uid = int(status['Uid'].split()[0])
    executable = readlink(base/'exe')
    info.update(pid=pid, uid=uid, user=username(uid), exe=executable,
                name=os.path.basename(executable).removesuffix(' (deleted)') or info['name'])
    # Protect the serving WebTerm and its PTY supervisor as well as ancestors.
    info['can_control'] = uid == os.geteuid() and pid not in protected and info['name'] not in ('webterm', 'webterm-runtime')
    io = key_values(safe_read(base/'io', 8192))
    info['read_bytes'] = int(io['read_bytes']) if io.get('read_bytes', '').isdigit() else None
    info['write_bytes'] = int(io['write_bytes']) if io.get('write_bytes', '').isdigit() else None
    if ports is not None:
        info['ports'], info['gpu_engine_ns'], info['gpu_memory_bytes'] = process_fds(base, ports)
    return info


def run_nvidia(args, timeout=2):
    binary = shutil.which('nvidia-smi')
    if not binary:
        return ''
    try:
        p = subprocess.run([binary, *args], capture_output=True, text=True, timeout=timeout, check=False)
        return p.stdout[:MAX_FILE] if p.returncode == 0 else ''
    except (OSError, subprocess.TimeoutExpired):
        return ''


def parse_nvidia_devices(text):
    devices = []
    for values in csv.reader(text.splitlines(), skipinitialspace=True):
        if len(values) != 7:
            continue
        index, name, busy, used, total, temperature, pci = [v.strip() for v in values]
        devices.append({'id': 'nvidia:' + index, 'vendor': 'NVIDIA', 'name': name,
            'utilization_percent': number(busy), 'memory_used_bytes': int(float(used)*1048576) if number(used) is not None else None,
            'memory_total_bytes': int(float(total)*1048576) if number(total) is not None else None,
            'temperature_c': number(temperature), 'pci': pci, 'source': 'nvidia-smi'})
    return devices


def nvidia_metrics():
    devices = parse_nvidia_devices(run_nvidia(['--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,pci.bus_id', '--format=csv,noheader,nounits']))
    processes = {}
    if not devices:
        return devices, processes
    # pmon includes graphics and compute clients; '-' remains unavailable, not 0.
    for line in run_nvidia(['pmon', '-c', '1', '-s', 'um'], timeout=3).splitlines():
        parts = line.split()
        if len(parts) < 5 or not parts[0].isdigit() or not parts[1].isdigit():
            continue
        pid = int(parts[1]); busy = number(parts[3])
        item = processes.setdefault(pid, {'gpu_percent': None, 'gpu_memory_bytes': None})
        if busy is not None:
            item['gpu_percent'] = (item['gpu_percent'] or 0) + busy
    for values in csv.reader(run_nvidia(['--query-compute-apps=pid,used_gpu_memory', '--format=csv,noheader,nounits']).splitlines(), skipinitialspace=True):
        if len(values) == 2 and values[0].strip().isdigit() and number(values[1]) is not None:
            item = processes.setdefault(int(values[0]), {'gpu_percent': None, 'gpu_memory_bytes': None})
            item['gpu_memory_bytes'] = (item['gpu_memory_bytes'] or 0) + int(float(values[1])*1048576)
    return devices, processes


def drm_devices(root=SYS):
    result = []
    for card in sorted((root/'class/drm').glob('card[0-9]*')):
        if not re.fullmatch(r'card\d+', card.name):
            continue
        base = card/'device'
        vendor = safe_read(base/'vendor', 32).strip().lower()
        if vendor not in ('0x1002', '0x8086'):
            continue
        name = safe_read(base/'product_name', 512).strip()
        vendor_name = 'AMD' if vendor == '0x1002' else 'Intel'
        device = safe_read(base/'device', 32).strip()
        busy = number(safe_read(base/'gpu_busy_percent', 32).strip())
        used = number(safe_read(base/'mem_info_vram_used', 64).strip())
        total = number(safe_read(base/'mem_info_vram_total', 64).strip())
        result.append({'id': card.name, 'vendor': vendor_name,
            'name': name or vendor_name + ' GPU ' + device, 'utilization_percent': busy,
            'memory_used_bytes': int(used) if used is not None else None,
            'memory_total_bytes': int(total) if total is not None else None,
            'temperature_c': None, 'source': 'DRM sysfs/fdinfo'})
    return result


def snapshot(previous=None, parent=None):
    previous = previous or {}; now = time.monotonic()
    elapsed = now - previous.get('at', now)
    procstat = read(PROC/'stat')
    cpu = cpu_counters(procstat)
    lastcpu = previous.get('cpu', cpu)
    all_delta = cpu[0] - lastcpu[0]
    busy = 100.0 * (1 - (cpu[1] - lastcpu[1]) / all_delta) if all_delta > 0 else None
    mem = key_values(read(PROC/'meminfo'))
    total = int(mem['MemTotal'].split()[0]) * 1024
    available = int(mem['MemAvailable'].split()[0]) * 1024
    ports = ports_table()
    protected = protected_pids(parent or os.getppid())
    devices, nvidia = nvidia_metrics()
    devices += drm_devices()
    raw = {}; rows = []; scanned = 0
    pids = sorted(int(p.name) for p in PROC.iterdir() if p.name.isdigit())
    for pid in pids[:MAX_PROCESSES]:
        try:
            row = process_info(pid, protected, ports)
            scanned += 1
            key = str(pid) + ':' + row['start_time']
            old = previous.get('processes', {}).get(key, {})
            raw[key] = {k: row.get(k) for k in ('ticks', 'read_bytes', 'write_bytes', 'gpu_engine_ns')}
            rate = delta(row.pop('ticks'), old.get('ticks'), elapsed)
            row['cpu_percent'] = round(rate / CLOCK * 100, 2) if rate is not None else None
            row['memory_percent'] = round(row['memory_bytes'] / max(total, 1) * 100, 2)
            for key_ in ('read', 'write'):
                rate = delta(row[key_+'_bytes'], old.get(key_+'_bytes'), elapsed)
                row['disk_'+key_+'_bps'] = round(rate, 2) if rate is not None else None
            rate = delta(row.pop('gpu_engine_ns', None), old.get('gpu_engine_ns'), elapsed)
            row['gpu_percent'] = round(rate / 1e9 * 100, 2) if rate is not None else None
            if pid in nvidia:
                for key_, value in nvidia[pid].items():
                    if value is not None:
                        row[key_] = value
            rows.append(row)
        except (OSError, ValueError, KeyError, ProcessLookupError):
            continue
    disks = []
    seen = set()
    for folder in ('/', '/home', '/build'):
        try:
            device = os.stat(folder).st_dev
            if device in seen:
                continue
            seen.add(device); stat = os.statvfs(folder)
            disks.append({'path': folder, 'total_bytes': stat.f_blocks*stat.f_frsize,
                'used_bytes': (stat.f_blocks-stat.f_bfree)*stat.f_frsize,
                'available_bytes': stat.f_bavail*stat.f_frsize})
        except OSError:
            pass
    data = {'timestamp': time.time(), 'sample_interval_s': round(elapsed, 3),
        'cpu_percent': round(max(0, min(100, busy)), 2) if busy is not None else None,
        'logical_cpus': os.cpu_count(), 'memory_total_bytes': total,
        'memory_used_bytes': total-available, 'disks': disks, 'gpu_devices': devices,
        'processes': rows[:MAX_ROWS], 'total_processes': len(pids),
        'truncated': len(rows) > MAX_ROWS or len(pids) > MAX_PROCESSES,
        'notes': ['CPU per process uses 100% per logical core. Disk columns are I/O, not allocated file space.',
                  'Unavailable or permission-restricted measurements are null. Intel/AMD engine use requires kernel DRM fdinfo support.']}
    return {'data': data, 'state': {'at': now, 'cpu': cpu, 'processes': raw}}


def identity(pid, expected, parent):
    if type(pid) is not int or pid <= 1 or not isinstance(expected, str) or not expected.isdigit():
        raise ValueError('A valid PID and start_time are required')
    info = process_info(pid, protected_pids(parent or os.getppid()))
    if info['start_time'] != expected:
        raise ValueError('Process identity changed; refresh before acting')
    return info


def detail(pid, expected, parent):
    info = identity(pid, expected, parent)
    base = PROC/str(pid)
    info.pop('ticks', None)
    info.update(cwd=readlink(base/'cwd'), ports=process_fds(base, ports_table())[0],
        security_note='Environment variables and command-line secrets are not exposed.')
    try:
        info['open_fds'] = sum(1 for _ in (base/'fd').iterdir())
    except OSError:
        info['open_fds'] = None
    uptime = float(read(PROC/'uptime').split()[0])
    info['elapsed_s'] = max(0, uptime - int(expected)/CLOCK)
    return {'data': info}


def action(pid, expected, kind, nice, parent):
    info = identity(pid, expected, parent)
    if not info['can_control']:
        raise PermissionError('Only your own non-WebTerm processes may be controlled')
    if kind not in ('terminate', 'kill', 'priority'):
        raise ValueError('Unknown process action')
    if kind == 'priority':
        if type(nice) is not int or not -20 <= nice <= 19:
            raise ValueError('Priority must be an integer from -20 to 19')
        identity(pid, expected, parent)
        os.setpriority(os.PRIO_PROCESS, pid, nice)
        return {'data': {'ok': True, 'pid': pid, 'nice': os.getpriority(os.PRIO_PROCESS, pid)}}
    sig = signal.SIGTERM if kind == 'terminate' else signal.SIGKILL
    if hasattr(os, 'pidfd_open') and hasattr(signal, 'pidfd_send_signal'):
        fd = os.pidfd_open(pid)
        try:
            identity(pid, expected, parent)
            signal.pidfd_send_signal(fd, sig)
        finally:
            os.close(fd)
    else:
        identity(pid, expected, parent)
        os.kill(pid, sig)
    return {'data': {'ok': True, 'pid': pid, 'action': kind}}


def main():
    request = json.loads(sys.stdin.buffer.read(2*1024*1024))
    operation = request.get('operation')
    try:
        if operation == 'snapshot':
            result = snapshot(request.get('previous'), request.get('parent'))
        elif operation == 'detail':
            result = detail(request.get('pid'), request.get('start_time'), request.get('parent'))
        elif operation == 'action':
            result = action(request.get('pid'), request.get('start_time'), request.get('action'), request.get('nice'), request.get('parent'))
        else:
            raise ValueError('Unknown monitor operation')
    except PermissionError:
        result = {'error': 'Permission denied. Only owned processes can be controlled; raising priority may require administrator privileges.', 'status': 403}
    except (ProcessLookupError, FileNotFoundError):
        result = {'error': 'Process exited. Refresh the process list.', 'status': 404}
    except (ValueError, OSError, KeyError) as error:
        result = {'error': str(error)[:300], 'status': 400}
    print(json.dumps(result, separators=(',', ':'), allow_nan=False))


if __name__ == '__main__':
    main()
