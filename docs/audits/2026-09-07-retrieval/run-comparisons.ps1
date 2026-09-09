param(
 [Parameter(Mandatory=$true)][string]$Baseline,
 [Parameter(Mandatory=$true)][string]$Candidate,
 [Parameter(Mandatory=$true)][string]$Harness,
 [Parameter(Mandatory=$true)][string]$ModelCache,
 [Parameter(Mandatory=$true)][string]$HfHome,
 [Parameter(Mandatory=$true)][string]$OutputRoot
)
$ErrorActionPreference = 'Stop'
$baselineBinary = (Resolve-Path -LiteralPath $Baseline).Path
$candidateBinary = (Resolve-Path -LiteralPath $Candidate).Path
$harnessBinary = (Resolve-Path -LiteralPath $Harness).Path
if (Test-Path -LiteralPath $OutputRoot) { throw 'Choose a new output directory to preserve existing evidence.' }
New-Item -ItemType Directory -Path $OutputRoot | Out-Null
$outputDirectory = (Resolve-Path -LiteralPath $OutputRoot).Path
$auditRoot = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
$env:FASTEMBED_CACHE_DIR = (Resolve-Path -LiteralPath $ModelCache).Path
$env:HF_HOME = (Resolve-Path -LiteralPath $HfHome).Path
$env:HF_HUB_OFFLINE = '1'
$env:KIMETSU_USER_BRAIN = '0'
$env:KIMETSU_BRAIN_EMBEDDER = 'bge-small-en-v1.5'
$env:KIMETSU_DETECT_CONFLICTS = '0'
$env:KIMETSU_RESOLVE_CONFLICTS = '0'
Remove-Item Env:KBENCH_RERANK_FLOOR -ErrorAction SilentlyContinue
Remove-Item Env:KBENCH_RERANKER -ErrorAction SilentlyContinue
Remove-Item Env:KIMETSU_ABSTAIN_EVIDENCE -ErrorAction SilentlyContinue
$validation = "$PSScriptRoot/validation-frozen.json"
if ((Get-FileHash -LiteralPath $validation -Algorithm SHA256).Hash.ToLowerInvariant() -ne '1a09270e09a9521c38f9dca14dfebe4349fed0e10b1537a92857eb18d3f6bb6f') { throw 'Frozen validation changed' }
$experiments = @(
 @{name='multilingual-contract'; dataset="$PSScriptRoot/agent-memory-contract.json"; budget=2048},
 @{name='multilingual-development'; dataset="$PSScriptRoot/development-100.json"; budget=6000},
 @{name='multilingual-validation'; dataset=$validation; budget=6000}
)
foreach ($experiment in $experiments) {
 python "$auditRoot/bench/scripts/compare_brainbench.py" --kbench $harnessBinary --baseline $baselineBinary --candidate $candidateBinary --dataset $experiment.dataset --budget-tokens $experiment.budget --repeats 3 --out "$outputDirectory/verified-$($experiment.name)" --baseline-threads 0 --candidate-threads 0 --baseline-reranker ms-marco-tinybert-l-2-v2 --candidate-reranker mmarco-minilm-l12-v2-int8 --candidate-rerank-floor 0.55
 if ($LASTEXITCODE -ne 0) { throw "Comparison failed: $($experiment.name)" }
 $result = Get-Content -Raw -LiteralPath "$outputDirectory/verified-$($experiment.name)/comparison.json" | ConvertFrom-Json
 if ($result.status -ne 'complete' -or $result.comparison.baseline_errors -ne 0 -or $result.comparison.candidate_errors -ne 0 -or $result.comparison.unpaired_scenarios.Count -ne 0) { throw 'Comparison contains errors or unpaired scenarios' }
}
