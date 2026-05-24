param(
  [string]$Distro = "Ubuntu-24.04",
  [string]$Repo = "/mnt/e/Kimetsu",
  [string]$TaskLimit = "16",
  [string]$Concurrency = "2",
  [string]$Model = "gpt-5.5",
  [string]$JobName = "codex-kimetsu-mcp",
  [string]$CodexVersion = "0.128.0",
  [ValidateSet("optional", "required", "none")]
  [string]$KimetsuMode = "optional",
  [ValidateSet("cold_brain", "reactive_warm", "full_warm")]
  [string]$WarmPolicy = "full_warm",
  [switch]$Preflight
)

$script = "$Repo/kimetsu_harbor/run-codex-kimetsu-bench.sh"
$preflightValue = if ($Preflight) { "1" } else { "0" }
wsl -d $Distro -u root --cd $Repo -- bash -lc "PREFLIGHT_ONLY='$preflightValue' TASK_LIMIT='$TaskLimit' N_CONCURRENT='$Concurrency' CODEX_MODEL='$Model' CODEX_VERSION='$CodexVersion' JOB_NAME='$JobName' KIMETSU_MCP_MODE='$KimetsuMode' KIMETSU_BRAIN_WARM_POLICY='$WarmPolicy' bash '$script'"
