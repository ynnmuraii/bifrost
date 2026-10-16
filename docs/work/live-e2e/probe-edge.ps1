# probe-edge.ps1 — one edge push, then the core's own lines for the transition.
#
# Diagnostic only: pushes the pointer at the right edge, samples the cursor to tell the
# probe's own injected motion apart from a person's, polls `status` for a few seconds and
# prints the core log lines that mention an edge, a focus change or an InputBatch. Needs a
# running core (start-cores.ps1) — read-only towards both machines.
#
# Usage:
#   ./probe-edge.ps1
[CmdletBinding()]
param(
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$EvidenceDir
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"
Initialize-LiveInput

$core = Join-Path $Install 'bifrost.exe'
if (-not (Test-Path -LiteralPath $core)) { throw "missing $core" }
$log = Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-core.log'
$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

$source = @'
using System;
using System.Runtime.InteropServices;
public static class ProbeCursor {
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x; public int y; }
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT point);
}
'@
if (-not [System.Management.Automation.PSTypeName]'ProbeCursor'.Type) { Add-Type -TypeDefinition $source }

function Get-Point {
    $point = New-Object ProbeCursor+POINT
    [void][ProbeCursor]::GetCursorPos([ref]$point)
    return "$($point.x),$($point.y)"
}

Write-Host "CURSOR_IDLE_SAMPLE=$((1..5 | ForEach-Object { Get-Point; Start-Sleep -Milliseconds 200 }) -join ' ')"
$offset = Get-FileSize -Path $log
$left = [LiveInput]::GetSystemMetrics(76); $top = [LiveInput]::GetSystemMetrics(77)
$width = [LiveInput]::GetSystemMetrics(78); $height = [LiveInput]::GetSystemMetrics(79)
Write-Host "VIRTUAL_DESK left=$left top=$top width=$width height=$height"
$y = $top + [int][Math]::Floor(($height - 1) / 2)
$edgeX = $left + $width - 1
Write-Host "PUSH_TO=$edgeX,$y"
Send-AbsolutePointer -X $edgeX -Y $y
Start-Sleep -Milliseconds 150
1..5 | ForEach-Object { [LiveInput]::Mouse(48, 0, 0x0001); Start-Sleep -Milliseconds 80 }
[LiveInput]::Mouse(240, 0, 0x0001)
Write-Host "PUSH_DONE=$(Get-Date -Format HH:mm:ss.fff) cursor=$(Get-Point)"
$lines = [System.Collections.Generic.List[string]]::new()
1..20 | ForEach-Object {
    $call = Invoke-Mdf -Exe $core -Arguments @('status')
    $line = ([regex]::Match($call.Text, '(?m)^OK .*$')).Value
    $text = "STATUS[$_]=$(Get-Date -Format HH:mm:ss.fff) cursor=$(Get-Point) $line"
    Write-Host $text
    $lines.Add($text)
    Start-Sleep -Milliseconds 150
}
$delta = Read-FileDelta -Path $log -Offset $offset
foreach ($line in ($delta -split "`n")) { if ($line -match 'link edge|focus=|InputBatch|came back|Remote|Band|band') { Write-Host "CORE $line" } }
Set-Content -LiteralPath (Join-Path $evidence 'probe-edge.txt') -Value $lines
Set-Content -LiteralPath (Join-Path $evidence 'probe-edge-core-delta.log') -Value $delta
