# verify-edge-disarm.ps1 — live check that a return disarms the edge it came home through.
#
# One return, then: a short continued push at the same edge must NOT re-enter (the
# anti-bounce hold, ~180 ms); a pause of ~250 ms without pushing must re-arm it; a retreat
# of ~10 logical px must re-arm it too. Reads only the PC core log.
# Needs the PC core running with the dev capture flag: run start-cores.ps1 -Dev first.
#
# Usage:
#   ./verify-edge-disarm.ps1
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
$pcCoreLog = Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-core.log'
$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

function Get-Status {
    $call = Invoke-Mdf -Exe $core -Arguments @('status')
    $match = [regex]::Match($call.Text, '(?m)^OK focus=.*$')
    if (-not $match.Success) { return "NO_STATUS: $($call.Text)" }
    return $match.Value
}

function Wait-Focus {
    param([string]$Focus, [int]$TimeoutSeconds = 20)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $status = ''
    do {
        $status = Get-Status
        if ($status -match "focus=$Focus\b") { return $status }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    return "TIMEOUT: $status"
}

function Enter-Remote {
    foreach ($direction in @('right', 'left')) {
        Send-EdgePush -Direction $direction
        $deadline = [DateTime]::UtcNow.AddSeconds(8)
        do {
            if ((Get-Status) -match 'focus=Remote') { return $direction }
            Start-Sleep -Milliseconds 200
        } while ([DateTime]::UtcNow -lt $deadline)
    }
    return 'none'
}

function Count-Of {
    param([string]$Text, [string]$Pattern)
    return @($Text -split "`n" | Where-Object { $_ -match $Pattern }).Count
}

$log = [System.Collections.Generic.List[string]]::new()
$write = { param([string]$line) $log.Add($line); Write-Host $line }

Write-Host (Wait-Focus -Focus 'Local' -TimeoutSeconds 30)
# The edge is only noticed while the agent holds the local observation: wait for it, not
# just for the focus line.
$deadline = [DateTime]::UtcNow.AddSeconds(30)
do { $status = Get-Status } while ($status -notmatch 'agent=ready' -and [DateTime]::UtcNow -lt $deadline)
& $write "AGENT_READY=$status"
if ((Get-Status) -match 'focus=Remote') { Send-Emergency; & $write (Wait-Focus -Focus 'Local' -TimeoutSeconds 20) }

$direction = Enter-Remote
& $write "ENTER_EDGE=$direction"
if ($direction -eq 'none') { throw 'no crossing; the edge never entered Remote' }
1..20 | ForEach-Object { [LiveInput]::Mouse(16, 4, 0x0001); Start-Sleep -Milliseconds 30 }
& $write ('DEEP=' + (Get-Status))
# The push that carried the pointer out is the *entry* direction; coming home means pushing
# the other way, and «the same direction» below is that homeward push.
$entryPush = [int]$(if ($direction -eq 'left') { -32 } else { 32 })
$back = -$entryPush

# One return, then a short continued push (≈200 ms) at the same edge: exactly one return and
# no re-entry is the anti-bounce; a re-entry here would be the ping-pong.
$offset = Get-FileSize -Path $pcCoreLog
for ($i = 1; $i -le 5; $i++) { [LiveInput]::Mouse($back, 0, 0x0001); Start-Sleep -Milliseconds 40 }
Start-Sleep -Milliseconds 200
$delta = Read-FileDelta -Path $pcCoreLog -Offset $offset
& $write "PUSHED_MS=$([Math]::Round(5 * 40 + 200, 0)) status=$(Get-Status)"
& $write "ANTIBOUNCE returns=$(Count-Of -Text $delta -Pattern 'came back over the entry edge') entries=$(Count-Of -Text $delta -Pattern 'reached a link edge')"
foreach ($line in @(($delta -split "`n") | Where-Object { $_ -match 'came back over the entry edge|reached a link edge' } | Select-Object -First 4)) { & $write "ANTIBOUNCE_LINE $line" }

# A pause without pushing (~250 ms re-arm) and back to the edge: the edge must be armed again.
Start-Sleep -Milliseconds 400
$offset2 = Get-FileSize -Path $pcCoreLog
for ($i = 1; $i -le 20; $i++) { [LiveInput]::Mouse($entryPush, 0, 0x0001); Start-Sleep -Milliseconds 40 }
Start-Sleep -Milliseconds 300
$delta2 = Read-FileDelta -Path $pcCoreLog -Offset $offset2
& $write "REARM_PAUSE status=$(Get-Status) entries=$(Count-Of -Text $delta2 -Pattern 'reached a link edge') returns=$(Count-Of -Text $delta2 -Pattern 'came back over the entry edge')"
foreach ($line in @(($delta2 -split "`n") | Where-Object { $_ -match 'came back over the entry edge|reached a link edge' } | Select-Object -First 3)) { & $write "REARM_PAUSE_LINE $line" }
if ((Get-Status) -match 'focus=Remote') {
    for ($i = 1; $i -le 20 -and (Get-Status) -match 'focus=Remote'; $i++) { [LiveInput]::Mouse($back, 0, 0x0001); Start-Sleep -Milliseconds 40 }
    & $write ('REARM_RETURN=' + (Get-Status))
}

# A retreat of ~10 logical px re-arms the returned edge just as the pause does: out again.
for ($i = 1; $i -le 2; $i++) { [LiveInput]::Mouse($back, 0, 0x0001); Start-Sleep -Milliseconds 40 }
$offset3 = Get-FileSize -Path $pcCoreLog
for ($i = 1; $i -le 20; $i++) { [LiveInput]::Mouse($entryPush, 0, 0x0001); Start-Sleep -Milliseconds 40 }
Start-Sleep -Milliseconds 300
$delta3 = Read-FileDelta -Path $pcCoreLog -Offset $offset3
& $write "REARM_RETREAT status=$(Get-Status) entries=$(Count-Of -Text $delta3 -Pattern 'reached a link edge') returns=$(Count-Of -Text $delta3 -Pattern 'came back over the entry edge')"
foreach ($line in @(($delta3 -split "`n") | Where-Object { $_ -match 'came back over the entry edge|reached a link edge' } | Select-Object -First 3)) { & $write "REARM_RETREAT_LINE $line" }

if ((Get-Status) -match 'focus=Remote') { Send-Emergency; & $write ('BACK_TO_LOCAL=' + (Wait-Focus -Focus 'Local' -TimeoutSeconds 20)) }
& $write ('FINAL=' + (Get-Status))
Set-Content -LiteralPath (Join-Path $evidence 'verify-edge-disarm.txt') -Value $log
