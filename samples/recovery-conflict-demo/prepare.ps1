param(
    [string]$OutputRoot = (Join-Path $PSScriptRoot "generated")
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$resolvedOutputRoot = [System.IO.Path]::GetFullPath($OutputRoot)
$suffix = "{0}-{1}" -f (Get-Date -Format "yyyyMMdd-HHmmss"), ([Guid]::NewGuid().ToString("N").Substring(0, 8))
$demoRoot = Join-Path $resolvedOutputRoot "RecoveryConflictDemo-$suffix"
$projectPath = Join-Path $demoRoot "UnityProject"

New-Item -ItemType Directory -Path $resolvedOutputRoot -Force | Out-Null
$previousDemoProject = [Environment]::GetEnvironmentVariable("CHECKPO_RECOVERY_DEMO_PROJECT", "Process")
$previousDataRoot = [Environment]::GetEnvironmentVariable("CHECKPO_DATA_DIR", "Process")
[Environment]::SetEnvironmentVariable("CHECKPO_RECOVERY_DEMO_PROJECT", $projectPath, "Process")
[Environment]::SetEnvironmentVariable("CHECKPO_DATA_DIR", $null, "Process")

Push-Location $repoRoot
try {
    cargo test -p checkpo-core transaction::tests::prepare_manual_recovery_conflict_demo --locked -- --ignored --exact --nocapture
    if ($LASTEXITCODE -ne 0) {
        throw "復旧デモの生成に失敗しました。cargo test の出力を確認してください。"
    }
}
finally {
    Pop-Location
    [Environment]::SetEnvironmentVariable("CHECKPO_RECOVERY_DEMO_PROJECT", $previousDemoProject, "Process")
    [Environment]::SetEnvironmentVariable("CHECKPO_DATA_DIR", $previousDataRoot, "Process")
}

Write-Host ""
Write-Host "復旧デモを作成しました。" -ForegroundColor Green
Write-Host "CheckPoで次のフォルダーを選択してください:"
Write-Host $projectPath -ForegroundColor Cyan
Write-Host ""
Write-Host "選択後の操作:"
Write-Host "  1. 『復旧する』を押す"
Write-Host "  2. DemoAvatar.prefabとmetaがチェックポイント版へ戻ることを確認する"
Write-Host "  ※ Unityが中断後に保存した版はCheckPo内へ安全コピーされます"
