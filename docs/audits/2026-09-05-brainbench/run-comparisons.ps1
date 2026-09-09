param(
    [Parameter(Mandatory=$true)][string]$Baseline,
    [Parameter(Mandatory=$true)][string]$Candidate,
    [Parameter(Mandatory=$true)][string]$Harness,
    [Parameter(Mandatory=$true)][string]$ModelCache,
    [Parameter(Mandatory=$true)][string]$OutputRoot
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
$runner = Join-Path $repoRoot 'bench/scripts/compare_brainbench.py'
$harness = (Resolve-Path -LiteralPath $Harness).Path
$baselineBinary = (Resolve-Path -LiteralPath $Baseline).Path
$candidateBinary = (Resolve-Path -LiteralPath $Candidate).Path
$contractFixture = Join-Path $PSScriptRoot 'agent-memory-contract.json'
$developmentFixture = Join-Path $PSScriptRoot 'development-100.json'
if (Test-Path -LiteralPath $OutputRoot) { throw 'Choose a new OutputRoot to preserve previous measurements.' }
New-Item -ItemType Directory -Path $OutputRoot | Out-Null
$auditOutput = (Resolve-Path -LiteralPath $OutputRoot).Path
$env:FASTEMBED_CACHE_DIR = (Resolve-Path -LiteralPath $ModelCache).Path
$env:HF_HUB_OFFLINE = '1'
$env:KIMETSU_USER_BRAIN = '0'
$env:KIMETSU_BRAIN_EMBEDDER = 'bge-small-en-v1.5'
$env:KIMETSU_DETECT_CONFLICTS = '0'
$env:KIMETSU_RESOLVE_CONFLICTS = '0'
Remove-Item Env:KBENCH_RERANKER -ErrorAction SilentlyContinue
Remove-Item Env:KIMETSU_ABSTAIN_EVIDENCE -ErrorAction SilentlyContinue

# Sequential experiments only: do not compile, test, or run other inference
# while this script owns the performance window. Artifacts fingerprint inputs.
$experiments = @(
    @{ name='contract-512'; dataset=$contractFixture; budget=512; baseline=$baselineBinary; candidate=$candidateBinary; extra=@('--baseline-threads','0','--candidate-threads','0') },
    @{ name='contract-2048'; dataset=$contractFixture; budget=2048; baseline=$baselineBinary; candidate=$candidateBinary; extra=@('--baseline-threads','0','--candidate-threads','0') },
    @{ name='development-6000'; dataset=$developmentFixture; budget=6000; baseline=$baselineBinary; candidate=$candidateBinary; extra=@('--baseline-threads','0','--candidate-threads','0') },
    @{ name='threads-default-vs-4'; dataset=$developmentFixture; budget=6000; baseline=$candidateBinary; candidate=$candidateBinary; extra=@('--baseline-threads','0','--candidate-threads','4','--baseline-reranker','ms-marco-tinybert-l-2-v2','--candidate-reranker','ms-marco-tinybert-l-2-v2') },
    @{ name='tinybert-vs-minilm-4threads'; dataset=$developmentFixture; budget=6000; baseline=$candidateBinary; candidate=$candidateBinary; extra=@('--baseline-threads','4','--candidate-threads','4','--baseline-reranker','ms-marco-tinybert-l-2-v2','--candidate-reranker','ms-marco-minilm-l-4-v2') }
)
foreach ($experiment in $experiments) {
    Write-Output "Starting $($experiment.name) at $([DateTimeOffset]::Now.ToString('o'))"
    $comparisonArgs = @($runner,'--kbench',$harness,'--baseline',$experiment.baseline,'--candidate',$experiment.candidate,'--dataset',$experiment.dataset,'--budget-tokens',"$($experiment.budget)",'--repeats','3','--out',"$auditOutput/paired-$($experiment.name)") + $experiment.extra
    python @comparisonArgs
    if ($LASTEXITCODE -ne 0) { throw "Comparison $($experiment.name) failed ($LASTEXITCODE); inspect preserved artifacts before continuing." }
    $pairedResult = Get-Content -Raw -LiteralPath "$auditOutput/paired-$($experiment.name)/comparison.json" | ConvertFrom-Json
    if ($pairedResult.status -ne 'complete' -or $pairedResult.comparison.baseline_errors -ne 0 -or $pairedResult.comparison.candidate_errors -ne 0 -or $pairedResult.comparison.unpaired_scenarios.Count -ne 0) {
        throw "Comparison $($experiment.name) contains errors or unpaired scenarios; inspect artifacts before continuing."
    }
}
python "$PSScriptRoot/benchmark_idf_sql.py" --out "$auditOutput/idf-sql-comparison.json"
if ($LASTEXITCODE -ne 0) { throw "IDF helper experiment failed ($LASTEXITCODE)" }
