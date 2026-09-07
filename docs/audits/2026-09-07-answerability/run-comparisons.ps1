param(
 [Parameter(Mandatory=$true)][string]$Binary,
 [Parameter(Mandatory=$true)][string]$Harness,
 [Parameter(Mandatory=$true)][string]$OutputRoot,
 [string]$ModelCache='E:/Kimetsu/.fastembed_cache',
 [string]$HfHome,
 [switch]$TimingFollowup
)
$ErrorActionPreference = 'Stop'
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$harnessPath = (Resolve-Path -LiteralPath $Harness).Path
if (Test-Path -LiteralPath $OutputRoot) { throw 'Use a new output directory.' }
New-Item -ItemType Directory -Path $OutputRoot | Out-Null
$outputPath = (Resolve-Path -LiteralPath $OutputRoot).Path
$repo = (Resolve-Path "$PSScriptRoot/../../..").Path
$env:FASTEMBED_CACHE_DIR=(Resolve-Path -LiteralPath $ModelCache).Path
if (-not $HfHome) { $HfHome="$repo/tmp-tests/hf-home" }
$env:HF_HOME=(Resolve-Path -LiteralPath $HfHome).Path
$env:HF_HUB_OFFLINE='1'
$env:KIMETSU_USER_BRAIN='0'
$env:KIMETSU_BRAIN_EMBEDDER='bge-small-en-v1.5'
$env:KIMETSU_DETECT_CONFLICTS='0'
$env:KIMETSU_RESOLVE_CONFLICTS='0'
Remove-Item Env:KIMETSU_ABSTAIN_EVIDENCE -ErrorAction SilentlyContinue
$fixture="$PSScriptRoot/validation-frozen.json"
if ((Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'ea4452872956beed1030ec071572db89435780474180b17a7bf7fe8f481e5d7e') { throw 'Frozen validation changed' }
$experiments=@(
 @{name='development'; fixture="$PSScriptRoot/../2026-09-07-retrieval/development-100.json"; model='ms-marco-tinybert-l-2-v2'; floor=0.30},
 @{name='missing-fact-development'; fixture="$PSScriptRoot/../2026-09-07-retrieval/validation-frozen.json"; model='mmarco-minilm-l12-v2-int8'; floor=0.55},
 @{name='validation'; fixture=$fixture; model='mmarco-minilm-l12-v2-int8'; floor=0.55}
)
foreach ($experiment in $experiments) {
 python "$repo/bench/scripts/compare_brainbench.py" --kbench $harnessPath --baseline $binaryPath --candidate $binaryPath --dataset $experiment.fixture --budget-tokens 6000 --repeats 1 --out "$outputPath/$($experiment.name)" --baseline-threads 0 --candidate-threads 0 --baseline-reranker $experiment.model --candidate-reranker $experiment.model --baseline-rerank-floor $experiment.floor --candidate-rerank-floor $experiment.floor --baseline-explicit-fact-guard false --candidate-explicit-fact-guard true
 if ($LASTEXITCODE -ne 0) { throw "Comparison failed: $($experiment.name)" }
}

if ($TimingFollowup) {
 python "$repo/bench/scripts/compare_brainbench.py" --kbench $harnessPath --baseline $binaryPath --candidate $binaryPath --dataset "$PSScriptRoot/../2026-09-07-retrieval/validation-frozen.json" --budget-tokens 6000 --repeats 2 --out "$outputPath/missing-fact-timing-followup" --baseline-threads 0 --candidate-threads 0 --baseline-reranker mmarco-minilm-l12-v2-int8 --candidate-reranker mmarco-minilm-l12-v2-int8 --baseline-rerank-floor 0.55 --candidate-rerank-floor 0.55 --baseline-explicit-fact-guard false --candidate-explicit-fact-guard true
 if ($LASTEXITCODE -ne 0) { throw 'Timing follow-up failed' }
}
