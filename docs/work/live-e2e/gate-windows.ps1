# gate-windows.ps1 — the Windows side of the workspace gate.
#
# Mirrors .github/workflows/ci.yml: fmt, clippy (-D warnings), licence policy, a workspace
# build and the workspace tests (which include the agent's no-network test). The old
# link-time winsock import check is gone on purpose: the one bifrost executable links
# Winsock for the core, so the agent's no-socket rule is proved by
# apps/lws-agent/tests/no_network.rs instead (see that file's header).
#
# Usage:
#   ./gate-windows.ps1
#   ./gate-windows.ps1 -PlanOnly
[CmdletBinding()]
param(
    [string]$EvidenceDir,
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"
$root = Get-LwsRoot

if ($PlanOnly) {
    Write-PlanHeader 'gate-windows.ps1'
    Write-Host 'runs: cargo fmt --all --check; cargo clippy --workspace --all-targets -- -D warnings;'
    Write-Host '      cargo deny --all-features check licenses; cargo build --workspace; cargo test --workspace'
    return
}

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"
Start-Transcript -Path (Join-Path $evidence 'gate-windows.txt') -Force
try {
    Push-Location $root
    try {
        cargo fmt --all --check; if ($LASTEXITCODE) { throw 'fmt failed' }
        cargo clippy --workspace --all-targets -- -D warnings; if ($LASTEXITCODE) { throw 'clippy failed' }
        cargo deny --all-features check licenses; if ($LASTEXITCODE) { throw 'licenses failed' }
        cargo build --workspace; if ($LASTEXITCODE) { throw 'build failed' }
        cargo test --workspace; if ($LASTEXITCODE) { throw 'tests failed' }
        Write-Host 'PASS Windows full gate'
    } finally { Pop-Location }
} catch {
    Write-Error $_
    exit 1
} finally { Stop-Transcript }
