#!/usr/bin/env python3
"""RSS watchdog: run CMD, sample the total RSS of its process tree every
INTERVAL s, SIGKILL only that tree if it exceeds LIMIT. Used because the box
has no systemd and the cgroup tree is root-owned (no memory.max).

    memcap.py --limit-mib 5120 --interval 2 --log FILE -- CMD...
Exit: CMD's exit code, or 137 if the watchdog killed the tree.
"""
import argparse, os, signal, subprocess, sys, time

def ppid_map():
    kids = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat") as f:
                s = f.read()
            ppid = int(s[s.rindex(")") + 2:].split()[1])
        except (OSError, ValueError):
            continue
        kids.setdefault(ppid, []).append(int(d))
    return kids

def tree(root):
    kids, out, todo = ppid_map(), [], [root]
    while todo:
        p = todo.pop(); out.append(p); todo.extend(kids.get(p, []))
    return out

def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit-mib", type=int, required=True)
    ap.add_argument("--interval", type=float, default=2.0)
    ap.add_argument("--log", required=True)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    log = open(a.log, "a", buffering=1)
    stamp = lambda: time.strftime("%Y-%m-%d %H:%M:%S %Z")
    child = subprocess.Popen(cmd, start_new_session=True)
    limit = a.limit_mib * 1024
    log.write(f"[{stamp()}] memcap watchdog pid={os.getpid()} child={child.pid} limit={a.limit_mib}MiB interval={a.interval}s cmd={cmd}\n")
    peak = 0; last = 0.0
    while child.poll() is None:
        pids = tree(child.pid)
        total = sum(rss_kib(p) for p in pids)
        peak = max(peak, total)
        if total > limit:
            log.write(f"[{stamp()}] KILL: tree RSS {total // 1024} MiB > {a.limit_mib} MiB; SIGKILL {len(pids)} pids {pids}\n")
            for p in tree(child.pid):
                try: os.kill(p, signal.SIGKILL)
                except OSError: pass
            child.wait()
            log.write(f"[{stamp()}] killed; peak {peak // 1024} MiB\n")
            return 137
        if time.time() - last >= 60:
            log.write(f"[{stamp()}] tree rss {total // 1024} MiB ({len(pids)} pids), peak {peak // 1024} MiB\n"); last = time.time()
        time.sleep(a.interval)
    log.write(f"[{stamp()}] child exited rc={child.returncode}; peak tree RSS {peak // 1024} MiB\n")
    return child.returncode

if __name__ == "__main__":
    sys.exit(main())
