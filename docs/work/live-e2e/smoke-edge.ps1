# smoke-edge.ps1 — click/keys smoke over the screen edge, with edge-counter evidence.
#
# Enters Remote over a vertical edge, sends 5 left clicks (down+up 50 ms apart, 300 ms
# between clicks) right after the entry and 5 more after 3 s, then 3 key down/up edges,
# returns over the entry edge, and sends the emergency hotkey. The evidence is the
# per-stage `EdgeCounters` of the `lease liveness diagnostics` line: the PC hook, the
# CapturedBatch the core received, the transport InputBatch tx/rx, the receiver's Inject
# and the applied (EIS) edges the agent reported back.
#
# Needs the PC core started with the dev capture flag: run start-cores.ps1 -Dev first.
#
# Usage:
#   ./smoke-edge.ps1
#   ./smoke-edge.ps1 -EvidenceDir target/live-e2e/runs/20261014-120000
[CmdletBinding()]
param(
    [string]$Laptop = 'lws-laptop',
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$LaptopLogDir = '/home/ynnmuraii/.local/state/lan-workspace/logs',
    [string]$EvidenceDir
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"
Initialize-LiveInput

$core = Join-Path $Install 'bifrost.exe'
if (-not (Test-Path -LiteralPath $core)) { throw "missing $core" }
$pcCoreLog = Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-core.log'
$pcAgentLog = Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-agent.log'
$laptopCoreLog = "$LaptopLogDir/lws-core.log"
$laptopAgentLog = "$LaptopLogDir/lws-agent.log"

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

function Enter-Remote {
    param([int]$TimeoutSeconds = 10)
    foreach ($direction in @('right', 'left')) {
        Send-EdgePush -Direction $direction
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        do {
            if ((Get-MdfStatus -Exe $core) -match 'focus=Remote') {
                Write-Host "ENTER_EDGE=$direction"
                return $direction
            }
            Start-Sleep -Milliseconds 100
        } while ([DateTime]::UtcNow -lt $deadline)
    }
    throw 'neither edge entered Remote'
}

# Back out through the edge we entered by: the receiver's pointer sits just inside that
# edge, so pushing back out is what ends the lease.
function Return-Local {
    param([string]$EntryDir, [int]$TimeoutSeconds = 25)
    $delta = [int]$(if ($EntryDir -eq 'left') { 32 } else { -32 })
    foreach ($sign in @(1, -1)) {
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        do {
            [LiveInput]::Mouse($delta * $sign, 0, 0x0001)
            Start-Sleep -Milliseconds 60
            if ((Get-MdfStatus -Exe $core) -match 'focus=Local') { return }
        } while ([DateTime]::UtcNow -lt $deadline)
    }
    throw 'the lease did not return to Local'
}

# One `EdgeCounters` field of a diagnostics line. Both the core's own counters and the
# prost `protocol::ipc` ones render as `EdgeCounters { keys_down: N, keys_up: N,
# buttons_down: N, buttons_up: N }`; the ipc ones are inside `Some(...)`.
function Get-Edges {
    param([string]$Line, [string]$Field)
    if (-not $Line) { return $null }
    $match = [regex]::Match($Line, "$Field[=:] ?(?:Some\()?EdgeCounters \{ keys_down: (\d+), keys_up: (\d+), buttons_down: (\d+), buttons_up: (\d+) \}")
    if (-not $match.Success) { return $null }
    return [pscustomobject]@{
        keys_down = [int64]$match.Groups[1].Value
        keys_up = [int64]$match.Groups[2].Value
        buttons_down = [int64]$match.Groups[3].Value
        buttons_up = [int64]$match.Groups[4].Value
    }
}

function Format-Edges {
    param($Edges)
    if (-not $Edges) { return 'absent' }
    return "keys_down=$($Edges.keys_down) keys_up=$($Edges.keys_up) buttons_down=$($Edges.buttons_down) buttons_up=$($Edges.buttons_up)"
}

function Get-Balance {
    param($Edges)
    if (-not $Edges) { return 'absent' }
    if ($Edges.keys_down -eq $Edges.keys_up -and $Edges.buttons_down -eq $Edges.buttons_up) { return 'balanced' }
    return 'UNBALANCED'
}

function Report-Stage {
    param([string]$Stage, [string]$Line, [string]$Field)
    $edges = Get-Edges -Line $Line -Field $Field
    Write-Host "STAGE $Stage [$Field] $(Get-Balance $edges) $(Format-Edges $edges)"
    return $edges
}

try {
    Write-Host ('SMOKE_START=' + [DateTime]::UtcNow.ToString('o'))
    Write-Host (Get-MdfStatus -Exe $core)
    if ((Get-MdfStatus -Exe $core) -match 'focus=Remote') { Send-Emergency; [void](Wait-MdfFocus -Exe $core -Focus 'Local' -TimeoutSeconds 20) }
    $pcOffset = Get-FileSize -Path $pcCoreLog
    $pcAgentOffset = Get-FileSize -Path $pcAgentLog
    $lapCoreOffset = Get-LaptopFileSize -Laptop $Laptop -Path $laptopCoreLog
    $lapAgentOffset = Get-LaptopFileSize -Laptop $Laptop -Path $laptopAgentLog

    $entryDir = Enter-Remote
    Write-Host ('REMOTE_ENTERED=' + [DateTime]::UtcNow.ToString('o'))
    1..5 | ForEach-Object { Send-Click; Start-Sleep -Milliseconds 300 }
    Write-Host ('CLICKS_FIRST=' + [DateTime]::UtcNow.ToString('o'))
    Start-Sleep -Seconds 3
    1..5 | ForEach-Object { Send-Click; Start-Sleep -Milliseconds 300 }
    Write-Host ('CLICKS_SECOND=' + [DateTime]::UtcNow.ToString('o'))
    1..3 | ForEach-Object { Send-Key -Scan 0x1E; Start-Sleep -Milliseconds 50; Send-Key -Scan 0x1E -Up; Start-Sleep -Milliseconds 250 }
    Write-Host ('KEYS_DONE=' + [DateTime]::UtcNow.ToString('o'))
    Write-Host ('BEFORE_RETURN=' + (Get-MdfStatus -Exe $core))

    Return-Local -EntryDir $entryDir
    Write-Host ('RETURN_DONE=' + [DateTime]::UtcNow.ToString('o') + ' ' + (Get-MdfStatus -Exe $core))
    Send-Emergency
    Start-Sleep -Milliseconds 500
    Write-Host ('EMERGENCY_SENT=' + [DateTime]::UtcNow.ToString('o') + ' ' + (Get-MdfStatus -Exe $core))

    $pcDelta = Read-FileDelta -Path $pcCoreLog -Offset $pcOffset
    $pcAgentDelta = Read-FileDelta -Path $pcAgentLog -Offset $pcAgentOffset
    $lapCoreDelta = Get-LaptopFileDelta -Laptop $Laptop -Path $laptopCoreLog -Offset $lapCoreOffset
    $lapAgentDelta = Get-LaptopFileDelta -Laptop $Laptop -Path $laptopAgentLog -Offset $lapAgentOffset
    Set-Content -LiteralPath (Join-Path $evidence 'pc-core-delta.log') -Value $pcDelta
    Set-Content -LiteralPath (Join-Path $evidence 'pc-agent-delta.log') -Value $pcAgentDelta
    Set-Content -LiteralPath (Join-Path $evidence 'laptop-core-delta.log') -Value $lapCoreDelta
    Set-Content -LiteralPath (Join-Path $evidence 'laptop-agent-delta.log') -Value $lapAgentDelta

    $pcDiag = @(Select-Matching -Text $pcDelta -Pattern 'lease liveness diagnostics')
    $lapDiag = @(Select-Matching -Text $lapCoreDelta -Pattern 'lease liveness diagnostics')
    Show-Lines -Name 'PC_DIAG' -Lines $pcDiag -Head 1 -Tail 1
    Show-Lines -Name 'LAPTOP_DIAG' -Lines $lapDiag -Head 1 -Tail 1
    if ($pcDiag.Count -eq 0 -or $lapDiag.Count -eq 0) { throw 'the lease diagnostics line never appeared in one of the cores' }
    $pcFinal = @($pcDiag | Where-Object { $_ -match 'final_snapshot=true' })
    $lapFinal = @($lapDiag | Where-Object { $_ -match 'final_snapshot=true' })
    if ($pcFinal.Count -eq 0 -or $lapFinal.Count -eq 0) { throw 'a Remote session ended without a final diagnostics snapshot' }
    $pcLine = $pcFinal[-1]
    $lapLine = $lapFinal[-1]
    $pcSecond = if ($pcDiag.Count -gt 1) { $pcDiag[-2] } else { $pcDiag[-1] }
    $lapSecond = if ($lapDiag.Count -gt 1) { $lapDiag[-2] } else { $lapDiag[-1] }
    Write-Host "PC_DIAG_LINES=$($pcDiag.Count) PC_FINAL_LINES=$($pcFinal.Count)"
    Write-Host "LAPTOP_DIAG_LINES=$($lapDiag.Count) LAPTOP_FINAL_LINES=$($lapFinal.Count)"

    Report-Stage -Stage 'PC_HOOK_PREVIOUS_SECOND' -Line $pcSecond -Field 'hook_edges' | Out-Null
    Report-Stage -Stage 'PC_CAPTURED_PREVIOUS_SECOND' -Line $pcSecond -Field 'captured_edges' | Out-Null
    Write-Host '--- final snapshot, both sides ---'
    $hook = Report-Stage -Stage 'PC_HOOK' -Line $pcLine -Field 'hook_edges'
    $captured = Report-Stage -Stage 'PC_CAPTURED' -Line $pcLine -Field 'captured_edges'
    $pcInject = Report-Stage -Stage 'PC_INJECT' -Line $pcLine -Field 'inject_edges'
    $pcApplied = Report-Stage -Stage 'PC_EIS_APPLIED' -Line $pcLine -Field 'eis_applied_edges'
    $pcTx = Report-Stage -Stage 'PC_INPUT_TX' -Line $pcLine -Field 'input_tx'
    $lapRx = Report-Stage -Stage 'LAPTOP_INPUT_RX' -Line $lapLine -Field 'input_rx'
    $lapHook = Report-Stage -Stage 'LAPTOP_HOOK' -Line $lapLine -Field 'hook_edges'
    $lapCaptured = Report-Stage -Stage 'LAPTOP_CAPTURED' -Line $lapLine -Field 'captured_edges'
    $lapInject = Report-Stage -Stage 'LAPTOP_INJECT' -Line $lapLine -Field 'inject_edges'
    $lapApplied = Report-Stage -Stage 'LAPTOP_EIS_APPLIED' -Line $lapLine -Field 'eis_applied_edges'

    $stages = @(
        @{ name = 'pc_hook'; edges = $hook }, @{ name = 'pc_captured'; edges = $captured },
        @{ name = 'pc_input_tx'; edges = $pcTx }, @{ name = 'laptop_input_rx'; edges = $lapRx },
        @{ name = 'laptop_inject'; edges = $lapInject }, @{ name = 'laptop_eis_applied'; edges = $lapApplied }
    )
    $unbalanced = @($stages | Where-Object { $_.edges -and ($_.edges.keys_down -ne $_.edges.keys_up -or $_.edges.buttons_down -ne $_.edges.buttons_up) })
    foreach ($stage in $stages) { Write-Host "CHECK $($stage.name): $(Get-Balance $stage.edges)" }
    $chain = @($stages | Where-Object { $_.edges })
    Write-Host "CHAIN_BALANCED=$($unbalanced.Count -eq 0) STAGES_WITH_COUNTERS=$($chain.Count)"
    if ($chain.Count -gt 0) {
        $first = $chain[0].edges
        $same = @($chain | Where-Object { $_.edges.keys_down -ne $first.keys_down -or $_.edges.keys_up -ne $first.keys_up -or $_.edges.buttons_down -ne $first.buttons_down -or $_.edges.buttons_up -ne $first.buttons_up })
        Write-Host "CHAIN_SAME_COUNTS=$($same.Count -eq 0)"
        foreach ($stage in $same) { Write-Host "CHAIN_DIFFERS $($stage.name) $(Format-Edges $stage.edges)" }
    }

    # The noisy line is DEBUG now: the request is that Local shows none at WARN.
    $warnDrops = @(Select-Matching -Text $pcDelta -Pattern 'WARN .*captured input outside a remote source lease')
    $debugDrops = @(Select-Matching -Text $pcDelta -Pattern 'DEBUG .*captured input outside a remote source lease')
    $warnAll = @(Select-Matching -Text $pcDelta -Pattern ' WARN ')
    $errorAll = @(Select-Matching -Text $pcDelta -Pattern ' ERROR ')
    Write-Host "PC_LEASE_DROP_WARN=$($warnDrops.Count) PC_LEASE_DROP_DEBUG=$($debugDrops.Count)"
    Write-Host "PC_DELTA_WARN_TOTAL=$($warnAll.Count) PC_DELTA_ERROR_TOTAL=$($errorAll.Count)"
    foreach ($line in $warnAll) { Write-Host "PC_DELTA_WARN $line" }
    foreach ($line in $errorAll) { Write-Host "PC_DELTA_ERROR $line" }
    $preparing = @(Select-Matching -Text $pcDelta -Pattern 'focus="Preparing"')
    Write-Host "PC_FOCUS_PREPARING_COUNT=$($preparing.Count)"
} finally {
    try {
        if ((Get-MdfStatus -Exe $core) -notmatch 'focus=Local') { Send-Emergency; [void](Wait-MdfFocus -Exe $core -Focus 'Local' -TimeoutSeconds 20) }
    } catch { Write-Host "SMOKE_RESET_FAILED=$_" }
    Write-Host ('SMOKE_FINISH=' + [DateTime]::UtcNow.ToString('o') + ' ' + (Get-MdfStatus -Exe $core))
}
