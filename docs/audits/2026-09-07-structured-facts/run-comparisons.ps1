param(
 [Parameter(Mandatory=$true)][string]$Baseline,
 [Parameter(Mandatory=$true)][string]$Candidate,
 [Parameter(Mandatory=$true)][string]$Harness,
 [Parameter(Mandatory=$true)][string]$OutputRoot,
 [string]$ModelCache='E:/Kimetsu/.fastembed_cache',
 [string]$HfHome
)
$ErrorActionPreference = 'Stop'
$baselinePath = (Resolve-Path -LiteralPath $Baseline).Path
$candidatePath = (Resolve-Path -LiteralPath $Candidate).Path
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
$env:KIMETSU_EMBED_DAEMON='0'
$env:KIMETSU_BRAIN_EMBEDDER='bge-small-en-v1.5'
$env:KIMETSU_DETECT_CONFLICTS='0'
$env:KIMETSU_RESOLVE_CONFLICTS='0'
Remove-Item Env:KIMETSU_ABSTAIN_EVIDENCE -ErrorAction SilentlyContinue
$fixture="$PSScriptRoot/validation-frozen.json"
if ((Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash.ToLowerInvariant() -ne '6b90f328e989fa0addf7c9a6d2468114fa1b233800f6e979a83830250dd4e469') { throw 'Frozen validation changed' }
if ((Get-FileHash -LiteralPath "$PSScriptRoot/answerability-gold.json" -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'd4d8b3f84282ea52515a1e9dc4fe7fe20f1e26ef45c4905cefcd984db0540445') { throw 'Frozen answerability gold changed' }
$experiments=@(
 @{name='development'; fixture="$PSScriptRoot/../2026-09-07-retrieval/development-100.json"; model='ms-marco-tinybert-l-2-v2'; floor=0.30; repeats=1},
 @{name='answerability-regression'; fixture="$PSScriptRoot/../2026-09-07-answerability/validation-frozen.json"; model='mmarco-minilm-l12-v2-int8'; floor=0.55; repeats=1},
 @{name='validation'; fixture=$fixture; model='mmarco-minilm-l12-v2-int8'; floor=0.55; repeats=2}
)
foreach ($experiment in $experiments) {
 python "$repo/bench/scripts/compare_brainbench.py" --kbench $harnessPath --baseline $baselinePath --candidate $candidatePath --dataset $experiment.fixture --budget-tokens 6000 --repeats $experiment.repeats --out "$outputPath/$($experiment.name)" --baseline-threads 0 --candidate-threads 0 --baseline-reranker $experiment.model --candidate-reranker $experiment.model --baseline-rerank-floor $experiment.floor --candidate-rerank-floor $experiment.floor --baseline-explicit-fact-guard true --candidate-explicit-fact-guard true
 if ($LASTEXITCODE -ne 0) { throw "Comparison failed: $($experiment.name)" }
}
