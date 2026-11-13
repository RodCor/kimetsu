#!/usr/bin/env python3
"""RSS watchdog: run CMD in its own session and sample the RSS of that
process group every INTERVAL seconds. SIGKILL the group if it exceeds LIMIT.

start_new_session runs setsid() in the child, so the child is the leader of
a new process group. Descendants that have been reparented stay in the group
and are still counted and killed. SIGTERM and SIGINT received by the watchdog
are forwarded to that group.

    memcap.py --limit-mib 5120 --interval 2 --log FILE -- CMD...
    memcap.py --self-test

Exit: CMD's exit code (128+signal when CMD dies from a signal), or 137 if
the watchdog killed the group.
"""
import argparse
import os
import signal
import subprocess
import sys
import tempfile
import time


def _pgrp(pid):
    try:
        with open(f"/proc/{pid}/stat") as handle:
            stat = handle.read()
        fields = stat[stat.rindex(")") + 2 :].split()
        return int(fields[2])
    except (OSError, ValueError, IndexError):
        return None


def group_pids(pgid):
    found = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        if _pgrp(pid) == pgid:
            found.append(pid)
    return found


def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status") as handle:
            for line in handle:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def kill_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except OSError:
        pass


def child_exit(rc):
    if rc is None:
        return 1
    if rc < 0:
        return 128 + (-rc)
    return rc


def _stamp():
    return time.strftime("%Y-%m-%d %H:%M:%S %Z")


def _reap(child, pgid):
    kill_group(pgid, signal.SIGKILL)
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        kill_group(pgid, signal.SIGKILL)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


def watch(cmd, limit_mib, interval, log_path):
    log = open(log_path, "a", buffering=1)
    try:
        return _watch(cmd, limit_mib, interval, log)
    finally:
        log.close()


def _watch(cmd, limit_mib, interval, log):
    # setsid() makes the child the leader of a new session and process group
    # (pgid == pid). os.killpg on that pgid covers the whole group.
    child = subprocess.Popen(cmd, start_new_session=True)
    pgid = child.pid
    forwarded = [None]

    def on_stop(signum, _frame):
        forwarded[0] = signum
        kill_group(pgid, signum)

    signal.signal(signal.SIGTERM, on_stop)
    signal.signal(signal.SIGINT, on_stop)
    limit = limit_mib * 1024
    log.write(
        f"[{_stamp()}] memcap watchdog pid={os.getpid()} child={child.pid} "
        f"pgid={pgid} limit={limit_mib}MiB interval={interval}s cmd={cmd}\n"
    )
    peak = 0
    last = 0.0
    while child.poll() is None:
        pids = group_pids(pgid)
        total = sum(rss_kib(pid) for pid in pids)
        peak = max(peak, total)
        if total > limit:
            log.write(
                f"[{_stamp()}] KILL: group RSS {total // 1024} MiB > {limit_mib} MiB; "
                f"SIGKILL pgid {pgid} ({len(pids)} pids {pids})\n"
            )
            _reap(child, pgid)
            log.write(f"[{_stamp()}] killed; peak {peak // 1024} MiB\n")
            return 137
        now = time.time()
        if now - last >= 60:
            log.write(
                f"[{_stamp()}] group rss {total // 1024} MiB ({len(pids)} pids), "
                f"peak {peak // 1024} MiB\n"
            )
            last = now
        time.sleep(interval)
    if forwarded[0] is not None:
        log.write(f"[{_stamp()}] forward signal {forwarded[0]} to pgid {pgid}\n")
    rc = child_exit(child.returncode)
    log.write(f"[{_stamp()}] child exited rc={rc}; peak group RSS {peak // 1024} MiB\n")
    return rc


def _state(pid):
    try:
        with open(f"/proc/{pid}/stat") as handle:
            stat = handle.read()
        return stat[stat.rindex(")") + 2 :].split()[0]
    except OSError:
        return None


def _alive(pid):
    return _state(pid) not in (None, "Z")


def _wait_dead(pids, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if all(not _alive(pid) for pid in pids):
            return True
        time.sleep(0.05)
    return False


def _python_rss_kib():
    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
    try:
        peak = 0
        for _ in range(10):
            time.sleep(0.05)
            peak = max(peak, rss_kib(proc.pid))
        return peak
    finally:
        proc.kill()
        proc.wait(timeout=5)


def _run(args, timeout):
    proc = subprocess.Popen(args)
    try:
        return proc.wait(timeout=timeout), proc
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
        raise


def _stop_pid(pid):
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass


def _read_pids(path):
    try:
        lines = [int(line) for line in open(path) if line.strip()]
    except (OSError, ValueError):
        return []
    return lines


def _fail(message, log_path):
    try:
        text = open(log_path).read()
    except OSError:
        text = ""
    raise SystemExit(f"{message}\n--- log ---\n{text}")


ALLOC = r"""
import os, sys, time
pidfile = sys.argv[1]
alloc_mib = int(sys.argv[2])
leader = os.getpid()
mid = os.fork()
if mid == 0:
    grand = os.fork()
    if grand > 0:
        os._exit(0)
    parent = os.getppid()
    deadline = time.time() + 5
    while os.getppid() == parent and time.time() < deadline:
        time.sleep(0.01)
    ppid = os.getppid()
    with open(pidfile, "w") as handle:
        handle.write(f"{leader}\n{os.getpid()}\n{ppid}\n")
        handle.flush()
        os.fsync(handle.fileno())
    chunks = []
    remaining = alloc_mib * 1024 * 1024
    while remaining > 0:
        size = min(remaining, 4 * 1024 * 1024)
        block = bytearray(size)
        view = memoryview(block)
        for off in range(0, size, 4096):
            view[off] = 1
        chunks.append(block)
        remaining -= size
    time.sleep(60)
    os._exit(0)
os.waitpid(mid, 0)
time.sleep(60)
"""

SIGWAIT = r"""
import os, sys, time
path = sys.argv[1]
leader = os.getpid()
pid = os.fork()
if pid == 0:
    with open(path, "w") as handle:
        handle.write(f"{leader}\n{os.getpid()}\n")
        handle.flush()
        os.fsync(handle.fileno())
    time.sleep(60)
    os._exit(0)
time.sleep(60)
"""


def _passthrough(code, log_path):
    rc, _proc = _run(
        [
            sys.executable,
            __file__,
            "--limit-mib",
            "256",
            "--interval",
            "0.2",
            "--log",
            log_path,
            "--",
            sys.executable,
            "-c",
            f"import sys; sys.exit({code})",
        ],
        timeout=15,
    )
    if rc != code:
        _fail(f"passthrough rc {rc}, expected {code}", log_path)


def _signal_forward(sig, log_path, pidfile):
    proc = subprocess.Popen(
        [
            sys.executable,
            __file__,
            "--limit-mib",
            "512",
            "--interval",
            "0.2",
            "--log",
            log_path,
            "--",
            sys.executable,
            "-c",
            SIGWAIT,
            pidfile,
        ],
        stderr=subprocess.DEVNULL,
    )
    pids = []
    try:
        deadline = time.time() + 5
        while time.time() < deadline:
            pids = _read_pids(pidfile)
            if len(pids) >= 2 and all(_alive(pid) for pid in pids[:2]):
                break
            if proc.poll() is not None:
                _fail(f"memcap exited {proc.returncode} before the child was ready", log_path)
            time.sleep(0.05)
        else:
            _fail(f"child did not record its pids: {pids}", log_path)
        os.kill(proc.pid, sig)
        rc = proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _stop_pid(proc.pid)
        proc.wait(timeout=5)
        _fail(f"memcap did not exit after signal {sig}", log_path)
    finally:
        _stop_pid(proc.pid)
        if pids:
            kill_group(pids[0], signal.SIGKILL)
    expected = 128 + sig
    if rc != expected:
        _fail(f"signal {sig} rc {rc}, expected {expected}", log_path)
    if not _wait_dead(pids[:2], 3):
        _fail(f"signal {sig} left pids {[pid for pid in pids[:2] if _alive(pid)]}", log_path)
    try:
        text = open(log_path).read()
    except OSError:
        text = ""
    if f"forward signal {sig}" not in text:
        _fail(f"log missing forward of signal {sig}", log_path)


def _over_cap(log_path, pidfile):
    baseline = _python_rss_kib()
    if baseline <= 0:
        raise SystemExit("could not read python VmRSS")
    baseline_mib = max(1, (baseline + 1023) // 1024)
    # Two forked interpreters can each report about `baseline` via VmRSS.
    # Stay above that, then require an allocation the parent process does
    # not make, so a ppid walk that misses the reparented child cannot trip.
    limit_mib = 2 * baseline_mib + 32
    alloc_mib = baseline_mib + 64
    print(
        f"memcap self-test: cap baseline={baseline_mib}MiB limit={limit_mib}MiB alloc={alloc_mib}MiB",
        flush=True,
    )
    proc = subprocess.Popen(
        [
            sys.executable,
            __file__,
            "--limit-mib",
            str(limit_mib),
            "--interval",
            "0.1",
            "--log",
            log_path,
            "--",
            sys.executable,
            "-c",
            ALLOC,
            pidfile,
            str(alloc_mib),
        ]
    )
    pids = []
    try:
        rc = proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        _stop_pid(proc.pid)
        proc.wait(timeout=5)
        _fail("over-cap child was not killed within 30s", log_path)
    finally:
        _stop_pid(proc.pid)
        pids = _read_pids(pidfile)
        if pids:
            kill_group(pids[0], signal.SIGKILL)
    if rc != 137:
        _fail(f"over-cap rc {rc}, expected 137", log_path)
    if len(pids) < 3:
        _fail(f"over-cap pidfile incomplete: {pids}", log_path)
    leader, grand, ppid = pids[0], pids[1], pids[2]
    if ppid in (leader, grand):
        _fail(f"grandchild was not reparented (leader={leader} grand={grand} ppid={ppid})", log_path)
    if not _wait_dead([leader, grand], 3):
        _fail(f"over-cap left live pids leader={_state(leader)} grand={_state(grand)}", log_path)
    live = [pid for pid in group_pids(leader) if _alive(pid)]
    if live:
        _fail(f"over-cap left process group {leader} with {live}", log_path)


def self_test():
    with tempfile.TemporaryDirectory(prefix="memcap-self-") as tmp:
        log_path = os.path.join(tmp, "memcap.log")
        print("memcap self-test: passthrough", flush=True)
        _passthrough(7, log_path)
        print("memcap self-test: SIGTERM", flush=True)
        _signal_forward(signal.SIGTERM, log_path, os.path.join(tmp, "term.pids"))
        print("memcap self-test: SIGINT", flush=True)
        _signal_forward(signal.SIGINT, log_path, os.path.join(tmp, "int.pids"))
        _over_cap(log_path, os.path.join(tmp, "cap.pids"))
    print("memcap self-test ok")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--limit-mib", type=int)
    parser.add_argument("--interval", type=float, default=2.0)
    parser.add_argument("--log")
    parser.add_argument("cmd", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.limit_mib is None or not args.log:
        parser.error("--limit-mib and --log are required")
    if args.limit_mib <= 0:
        parser.error("--limit-mib must be positive")
    if args.interval <= 0:
        parser.error("--interval must be positive")
    cmd = args.cmd[1:] if args.cmd and args.cmd[0] == "--" else list(args.cmd)
    if not cmd:
        parser.error("command required")
    return watch(cmd, args.limit_mib, args.interval, args.log)


if __name__ == "__main__":
    sys.exit(main())
