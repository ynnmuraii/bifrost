# apply-layout.ps1 — apply a layout TOML to the running core.
#
# Run only with a connected, Local owner and a freshly read full topology document. The CLI
# exposes `layout show/edit/apply/discard` but not GetTopology; the GUI IPC client is what
# produces this TOML. The core must already be running normal or dev (start-cores.ps1).
#
# Usage:
#   ./apply-layout.ps1 -LayoutToml target/live-e2e/layout-live.toml
#   ./apply-layout.ps1 -LayoutToml <file> -PlanOnly
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$LayoutToml,
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"

$core = Join-Path $Install 'bifrost.exe'
$layout = (Resolve-Path -LiteralPath $LayoutToml).Path
if (-not (Test-Path -LiteralPath $layout -PathType Leaf)) { throw "missing layout TOML: $layout" }

if ($PlanOnly) {
    Write-PlanHeader 'apply-layout.ps1'
    Write-Host "core:   $core"
    Write-Host "layout: $layout"
    Write-Host 'action: require `status` to be a connected Local owner, then `bifrost layout edit <file>`'
    return
}

if (-not (Test-Path -LiteralPath $core)) { throw "missing $core" }

$statusOutput = (Invoke-Mdf -Exe $core -Arguments @('status')).Text
$statusMatch = [regex]::Match($statusOutput, '(?m)^OK focus=.*$')
if (-not $statusMatch.Success -or $statusMatch.Value -notmatch '^OK focus=Local role=owner .*members=[1-9]\d*') {
    throw "refusing layout mutation: expected a connected Local owner; got: $statusOutput"
}
$result = Invoke-Mdf -Exe $core -Arguments @('layout', 'edit', $layout)
if ($result.Code -ne 0 -or $result.Text -notmatch '(?m)^OK applied revision=') {
    throw "layout edit failed: $($result.Text)"
}
Write-Host $result.Text.Trim()
