param(
  [string]$TaskLimit = "16",
  [string]$Concurrency = "2",
  [string]$Model = "gpt-5.5",
  [string]$CodexVersion = "0.128.0",
  [string]$JobPrefix = "codex-16x3",
  [string]$LogRoot = "kimetsu_harbor\benchmark-logs"
)

$ErrorActionPreference = "Continue"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$runner = Join-Path $PSScriptRoot "run-codex-kimetsu-wsl.ps1"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$logDir = Join-Path $repo (Join-Path $LogRoot "$JobPrefix-$stamp")
$summary = Join-Path $logDir "summary.txt"
$legs = @(
  @{ Mode = "required"; WarmPolicy = "full_warm" },
  @{ Mode = "optional"; WarmPolicy = "reactive_warm" },
  @{ Mode = "none"; WarmPolicy = "cold_brain" }
)

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
"Benchmark started: $(Get-Date -Format o)" | Set-Content -Path $summary
"TaskLimit=$TaskLimit Concurrency=$Concurrency Model=$Model CodexVersion=$CodexVersion" | Add-Content -Path $summary

foreach ($leg in $legs) {
  $mode = $leg.Mode
  $warmPolicy = $leg.WarmPolicy
  $jobName = "$JobPrefix-$mode-$stamp"
  $log = Join-Path $logDir "$mode.log"
  "`n=== $mode warm_policy=$warmPolicy $jobName start $(Get-Date -Format o) ===" | Add-Content -Path $summary

  try {
    & $runner `
      -TaskLimit $TaskLimit `
      -Concurrency $Concurrency `
      -Model $Model `
      -CodexVersion $CodexVersion `
      -KimetsuMode $mode `
      -WarmPolicy $warmPolicy `
      -JobName $jobName *> $log
    $exitCode = $LASTEXITCODE
    if ($null -eq $exitCode) {
      $exitCode = if ($?) { 0 } else { 1 }
    }
  } catch {
    $exitCode = 1
    $_ | Out-String | Add-Content -Path $log
  }

  "=== $mode exit=$exitCode end $(Get-Date -Format o) log=$log ===" | Add-Content -Path $summary
}

"Benchmark finished: $(Get-Date -Format o)" | Add-Content -Path $summary
"Logs: $logDir" | Add-Content -Path $summary
