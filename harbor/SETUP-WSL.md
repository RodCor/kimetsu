# WSL2 setup for Harbor + Kimetsu (MP-8)

A working setup proven during the MP-8 prep session. Use this when you
need to run `harbor run` against Terminal-Bench 2 from a Windows host
without Docker Desktop. Everything runs inside a dedicated Ubuntu 24.04
WSL distro so paths are native Linux and Harbor's Docker bind-mounts
work without translation.

## What we proved works

- Ubuntu 24.04 LTS in WSL2 (Python 3.12.3, Docker Engine 29.1.3,
  `docker compose` v2.40.3, systemd PID 1)
- Harbor 0.6.6 from PyPI
- Kimetsu Linux release binary, built from the repo at `/mnt/e/Kimetsu`
- Python adapter smoke (`harbor/smoke_test.py`) round-trips through the
  Linux binary cleanly (`KIMETSU_HARBOR_STUB=1`, 2 routed tool.exec
  frames + agent.done, exit 0)
- `harbor run -d terminal-bench/terminal-bench-2 -a oracle` makes real
  progress (4/89 with mean 0.750 in ~10 min on a cold cache; first run
  is slow because of image pulls)

## What did NOT work (record so we don't try again)

| approach | failure mode |
|----------|--------------|
| Docker Desktop via winget install | Stuck waiting on UAC elevation. We're not running elevated. |
| Docker installed in Ubuntu 20.04 + Harbor on Windows over `DOCKER_HOST=tcp://...` | Harbor's bind-mount of Windows paths `E:/Kimetsu/jobs/...` is rejected by Linux dockerd — there's no path translation layer outside Docker Desktop. |
| Docker installed in Ubuntu 20.04 + Harbor in Ubuntu 20.04 | Harbor needs Python 3.12; Ubuntu 20.04 ships 3.10. deadsnakes PPA install blocked by broken `python3-apt` module. |
| Docker daemon via systemd with `Requires=docker.socket` override-emptied | Daemon stopped between calls; `is-active` flipped to `inactive`. Manual `nohup dockerd` in a fresh distro is more reliable. |
| `tcp://localhost:2375` from Windows even with mirrored WSL2 networking | Connection refused intermittently. WSL VM IP works but isn't stable; the simplest path is to skip Windows-side Docker entirely. |

## Tested setup (Ubuntu 24.04 in WSL2)

### One-time install (run from elevated PowerShell only if WSL2 itself isn't installed)

```powershell
wsl --install -d Ubuntu-24.04 --no-launch
```

Everything below runs from a normal PowerShell — no elevation needed.

### Configure systemd + default user

```powershell
wsl -d Ubuntu-24.04 -u root -- bash -c "tee /etc/wsl.conf > /dev/null <<EOF
[boot]
systemd=true
[user]
default=root
EOF"
wsl --shutdown
```

### Install Docker + Compose v2 + Harbor + Rust

```powershell
wsl -d Ubuntu-24.04 -u root -- bash -c '
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
      python3-pip python3-venv \
      docker.io docker-compose-v2 \
      pkg-config libssl-dev build-essential
  pip install --break-system-packages --ignore-installed --quiet harbor
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
      sh -s -- -y --default-toolchain stable --profile minimal
  systemctl enable --now docker
'
```

### Verify

```powershell
wsl -d Ubuntu-24.04 -u root -- bash -c '
  echo "python: $(python3 --version)";
  echo "pip:    $(pip --version | head -1)";
  echo "docker: $(docker --version)";
  echo "compose: $(docker compose version | head -1)";
  echo "harbor: $(harbor --version 2>&1 | head -1)";
  echo "cargo:  $(source $HOME/.cargo/env && cargo --version)";
  echo "systemd: $(systemctl is-system-running)";
  echo "docker active: $(systemctl is-active docker)";
'
```

Expected output (versions may drift; the shape matters):

```
python: Python 3.12.3
pip:    pip 24.0 from /usr/lib/python3/dist-packages/pip (python 3.12)
docker: Docker version 29.1.3, build 29.1.3-0ubuntu3~24.04.2
compose: Docker Compose version 2.40.3+ds1-0ubuntu1~24.04.1
harbor: 0.6.6
cargo:  cargo 1.95.0 (...)
systemd: running
docker active: active
```

### Build the kimetsu Linux binary

```powershell
wsl -d Ubuntu-24.04 -u root --cd /mnt/e/Kimetsu -- bash -c '
  source $HOME/.cargo/env
  cargo build -p kimetsu-cli --release
'
```

Produces `target/release/kimetsu` (Linux ELF). The Windows
`target/release/kimetsu.exe` and the Linux `target/release/kimetsu` can
coexist in the same `target/release` dir.

### Smoke the adapter against the Linux binary

```powershell
wsl -d Ubuntu-24.04 -u root --cd /mnt/e/Kimetsu -- bash -c '
  KIMETSU_BIN=/mnt/e/Kimetsu/target/release/kimetsu \
  KIMETSU_HARBOR_STUB=1 \
    python3 harbor/smoke_test.py
'
```

Should print `[smoke] OK` with two tool.exec round-trips and exit 0.

### Smoke that Harbor + Docker actually grade Terminal-Bench

```powershell
wsl -d Ubuntu-24.04 -u root --cd /root -- bash -c '
  mkdir -p /root/harbor-jobs && cd /root/harbor-jobs
  harbor run -d terminal-bench/terminal-bench-2 -a oracle -n 1 -k 1 --yes
'
```

The oracle agent is built into Harbor and needs no API credentials.
Stop with Ctrl-C once you see the `0:00:30+` progress bar — that
confirms the stack is wired correctly. The first run downloads task
images (~hundreds of MB per task) and is slow; the cache is reused on
subsequent runs.

## Running the real v0.2 gauntlet (MP-8)

Once `CLAUDE_CODE_OAUTH_TOKEN` is in your environment:

```powershell
$token = (Read-Host -Prompt "CLAUDE_CODE_OAUTH_TOKEN" -AsSecureString)
$plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto(
    [Runtime.InteropServices.Marshal]::SecureStringToBSTR($token))
wsl -d Ubuntu-24.04 -u root --cd /mnt/e/Kimetsu -- bash -c "
  export CLAUDE_CODE_OAUTH_TOKEN='$plain'
  export PYTHONPATH=/mnt/e/Kimetsu
  export KIMETSU_BIN=/mnt/e/Kimetsu/target/release/kimetsu

  cd /root/harbor-jobs

  # a) bare claude-code baseline
  harbor run -d terminal-bench/terminal-bench-2 -a claude-code -m claude-haiku-4-5 -n 4 --yes --job-name bare

  # b) kimetsu, no brain
  KIMETSU_DISABLE_BROKER=1 harbor run -d terminal-bench/terminal-bench-2 \
    --agent-import-path harbor.kimetsu_agent:KimetsuAgent \
    -n 4 --yes --job-name kimetsu-no-brain

  # c) kimetsu with brain + curated memories
  harbor run -d terminal-bench/terminal-bench-2 \
    --agent-import-path harbor.kimetsu_agent:KimetsuAgent \
    -n 4 --yes --job-name kimetsu-brain
"
```

Per the V0.2 ship gate: repeat each mode 3× over ~1 week. Stable means
variance within ±5pp on accuracy across the 3 runs.

## Notes on workspace paths

The kimetsu repo lives on the Windows `E:` drive. Inside WSL it's
mounted at `/mnt/e/Kimetsu`. All `cargo build` / `harbor run` commands
operate on the WSL mount path. The Linux `target/release/kimetsu`
binary is what the adapter spawns; the Windows `kimetsu.exe` is for
local CLI use (`kimetsu brain memory review` etc.) and stays usable
from PowerShell unchanged.

## Daemon longevity

Once `systemctl enable docker` runs in 24.04, the docker daemon
restarts automatically when WSL boots. `wsl --shutdown` followed by any
`wsl -d Ubuntu-24.04` invocation brings the daemon back without manual
intervention. If you ever see "Docker daemon is not running" again,
`systemctl status docker` + `journalctl -u docker -n 30` are the first
two commands to run.
