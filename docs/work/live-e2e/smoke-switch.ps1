# smoke-switch.ps1 — lease-counter rounds without an edge crossing.
#
# `bifrost switch <peer>` starts the same Remote lease an edge crossing would, so the
# once-a-second `lease liveness diagnostics` line can be collected while the PC layout is
# unusable. Each round: Remote -> holdSeconds of small SendInput moves (every holdStepMs) ->
# emergency hotkey -> Local. The evidence is each core's diagnostics deltas and the sender/
# receiver counter pairs.
#
# The PC core must already run with the dev capture flag: run start-cores.ps1 -Dev first.
# The peer id comes from `bifrost peers`.
#
# Usage:
#   ./smoke-switch.ps1 -Peer cba880749e16084dbaeff4ed8e97b9a5f1e4a41c44c2beb93bee3269c5d4de21
#   ./smoke-switch.ps1 -Peer <id> -Rounds 1 -HoldSeconds 20
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Peer,
    [int]$Rounds = 3,
    [int]$HoldSeconds = 60,
    [int]$HoldStepMs = 2000,
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
$laptopCoreLog = "$LaptopLogDir/lws-core.log"
$laptopAgentLog = "$LaptopLogDir/lws-agent.log"

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

function Invoke-Core {
    param([string[]]$Arguments)
    return (Invoke-Mdf -Exe $core -Arguments $Arguments)
}

function Wait-Match {
    param([string]$Pattern, [int]$TimeoutSeconds, [string]$What)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $status = ''
    do {
        $status = Get-MdfStatus -Exe $core
        if ($status -match $Pattern) { return $status }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    Write-Host "WAIT_FAILED what=$What last=$status"
    return $status
}

# Only the `fsm=LeaseCounters { ... }` slice counts: transport counters repeat the same
# field names inside `transport_connection`.
function Get-Counter {
    param([string]$Line, [string]$Name)
    $slice = [regex]::Match($Line, 'fsm=LeaseCounters \{(.*?)\}').Groups[1].Value
    if (-not $slice) { return $null }
    $match = [regex]::Match($slice, [regex]::Escape($Name) + ':\s*(\d+)')
    if ($match.Success) { return [int64]$match.Groups[1].Value }
    return $null
}

function Show-Reasons {
    param([string]$Name, [string]$Text)
    $lines = Select-Matching -Text $Text -Pattern 'link down|LeaseExpired|focus focus="Releasing"|no display is known for the switch target'
    Write-Host "$Name REASON_LINES=$($lines.Count)"
    foreach ($line in @($lines | Select-Object -Last 3)) { Write-Host "$Name REASON $line" }
}

function Show-Pairs {
    param([string]$PcLine, [string]$LapLine)
    $pairs = @(
        @{ name = 'challenge laptop_sent:pc_received'; left = 'challenge_sent'; right = 'challenge_received' },
        @{ name = 'renew pc_sent:laptop_received'; left = 'renew_sent'; right = 'renew_received' },
        @{ name = 'status laptop_sent:pc_received'; left = 'status_sent'; right = 'status_received' }
    )
    foreach ($pair in $pairs) {
        $laptopSide = Get-Counter -Line $LapLine -Name $pair.left
        $pcSide = Get-Counter -Line $PcLine -Name $pair.right
        $delta = if ($null -ne $laptopSide -and $null -ne $pcSide) { $laptopSide - $pcSide } else { 'n/a' }
        Write-Host "DIVERGENCE $($pair.name) sender=$laptopSide receiver=$pcSide delta=$delta"
    }
    $laptopChallenges = Get-Counter -Line $LapLine -Name 'challenge_sent'
    $pcChallenges = Get-Counter -Line $PcLine -Name 'challenge_received'
    $laptopAccepts = Get-Counter -Line $LapLine -Name 'renew_accepted'
    $laptopRejects = Get-Counter -Line $LapLine -Name 'renew_rejected'
    $pcStatus = Get-Counter -Line $PcLine -Name 'status_received'
    $pcAccepts = Get-Counter -Line $PcLine -Name 'status_accepted'
    $pcRejects = Get-Counter -Line $PcLine -Name 'status_rejected'
    Write-Host "DIVERGENCE renewal_outcome laptop accepted=$laptopAccepts rejected=$laptopRejects ; source status_received=$pcStatus accepted=$pcAccepts rejected=$pcRejects"
    Write-Host "DIVERGENCE verdicts_source_sent_vs_receiver_seen challenges=$laptopChallenges/$pcChallenges"
}

try {
    Write-Host ('RUN_START=' + [DateTime]::UtcNow.ToString('o'))
    Write-Host (Wait-Match 'focus=Local.*role=owner.*members=1' 40 'local_owner_with_link')
    if ((Get-MdfStatus -Exe $core) -match 'focus=Remote') { Send-Emergency; [void](Wait-Match 'focus=Local' 20 'reset_local') }

    for ($round = 1; $round -le $Rounds; $round++) {
        $pcOffset = Get-FileSize -Path $pcCoreLog
        $lapOffset = Get-LaptopFileSize -Laptop $Laptop -Path $laptopCoreLog
        $lapAgentOffset = Get-LaptopFileSize -Laptop $Laptop -Path $laptopAgentLog
        $before = Wait-Match 'focus=Local.*role=owner' 30 'owner_before_round'
        Write-Host "ROUND_START=$round at=$([DateTime]::UtcNow.ToString('o')) $before"
        $after = ''
        foreach ($attempt in 1..3) {
            # A restarted core claims ownership a moment after it publishes Local; until it is
            # the owner the manual switch is refused with SWITCH_REFUSED NotOwner.
            $switched = Invoke-Core -Arguments @('switch', $Peer)
            Write-Host "SWITCH round=$round attempt=$attempt rc=$($switched.Code) $($switched.Text.Trim())"
            $after = Wait-Match 'focus=Remote' 6 "remote_round_${round}_attempt_$attempt"
            if ($after -match 'focus=Remote') { break }
            $owner = Wait-Match 'focus=Local.*role=owner' 15 'owner_retry'
            if ($owner -notmatch 'role=owner') { break }
        }
        Write-Host "ROUND_REMOTE=$round at=$([DateTime]::UtcNow.ToString('o')) $after"
        if ($after -notmatch 'focus=Remote') { Write-Host "ROUND_ABORT=$round switch did not reach Remote"; break }
        $holdStart = [DateTime]::UtcNow
        $holdKept = $true
        for ($i = 1; $i -le [int]($HoldSeconds * 1000 / $HoldStepMs); $i++) {
            [LiveInput]::Mouse(6, 4, 0x0001)
            Start-Sleep -Milliseconds $HoldStepMs
            if ($i % 5 -eq 0) {
                $status = Get-MdfStatus -Exe $core
                if ($status -notmatch 'focus=Remote') { $holdKept = $false; Write-Host "ROUND_HOLD_LOST=$round step=$i at=$([DateTime]::UtcNow.ToString('o')) $status"; break }
            }
        }
        $heldSeconds = [int]([DateTime]::UtcNow - $holdStart).TotalSeconds
        Write-Host "ROUND_HOLD_END=$round seconds=$heldSeconds $((Get-MdfStatus -Exe $core))"
        if ((Get-MdfStatus -Exe $core) -match 'focus=Remote') { Send-Emergency }
        $local = Wait-Match 'focus=Local' 20 "local_after_round_$round"
        Write-Host "ROUND_END=$round at=$([DateTime]::UtcNow.ToString('o')) $local"

        $pcText = Read-FileDelta -Path $pcCoreLog -Offset $pcOffset
        $lapText = Get-LaptopFileDelta -Laptop $Laptop -Path $laptopCoreLog -Offset $lapOffset
        $agentText = Get-LaptopFileDelta -Laptop $Laptop -Path $laptopAgentLog -Offset $lapAgentOffset
        Set-Content -LiteralPath (Join-Path $evidence "round$round-pc-core.log") -Value $pcText
        Set-Content -LiteralPath (Join-Path $evidence "round$round-laptop-core.log") -Value $lapText
        Set-Content -LiteralPath (Join-Path $evidence "round$round-laptop-agent.log") -Value $agentText

        $pcDiag = @(Select-Matching -Text $pcText -Pattern 'lease liveness diagnostics')
        $lapDiag = @(Select-Matching -Text $lapText -Pattern 'lease liveness diagnostics')
        $pcFinal = @(Select-Matching -Text $pcText -Pattern 'final_snapshot=true')
        $lapFinal = @(Select-Matching -Text $lapText -Pattern 'final_snapshot=true')
        $pcExpiry = @(Select-Matching -Text $pcText -Pattern 'LeaseExpired')
        $lapExpiry = @(Select-Matching -Text $lapText -Pattern 'LeaseExpired')
        $inject = @(Select-Matching -Text $agentText -Pattern 'agent InjectResult')
        Write-Host "ROUND_COUNTS round=$round pc_diag=$($pcDiag.Count) laptop_diag=$($lapDiag.Count) pc_final=$($pcFinal.Count) laptop_final=$($lapFinal.Count) pc_expiry=$($pcExpiry.Count) laptop_expiry=$($lapExpiry.Count) inject=$($inject.Count)"
        foreach ($line in @($pcDiag | Select-Object -First 2)) { Write-Host "PC_DIAG $line" }
        foreach ($line in @($pcDiag | Select-Object -Last 1)) { Write-Host "PC_DIAG_LAST $line" }
        foreach ($line in @($lapDiag | Select-Object -First 2)) { Write-Host "LAPTOP_DIAG $line" }
        foreach ($line in @($lapDiag | Select-Object -Last 1)) { Write-Host "LAPTOP_DIAG_LAST $line" }
        foreach ($line in @($pcFinal | Select-Object -Last 1)) { Write-Host "PC_FINAL $line" }
        foreach ($line in @($lapFinal | Select-Object -Last 1)) { Write-Host "LAPTOP_FINAL $line" }
        foreach ($line in $pcExpiry) { Write-Host "PC_EXPIRY $line" }
        foreach ($line in $lapExpiry) { Write-Host "LAPTOP_EXPIRY $line" }
        Show-Reasons -Name 'PC' -Text $pcText
        Show-Reasons -Name 'LAPTOP' -Text $lapText
        if ($inject.Count -gt 0) {
            foreach ($line in @($inject | Select-Object -First 1)) { Write-Host "INJECT_FIRST $line" }
            foreach ($line in @($inject | Select-Object -Last 1)) { Write-Host "INJECT_LAST $line" }
        }
        Write-Host "ROUND_VERDICT round=$round hold_seconds=$heldSeconds remote_kept=$holdKept inject_lines=$($inject.Count) pc_per_second=$($pcDiag.Count - $pcFinal.Count) laptop_per_second=$($lapDiag.Count - $lapFinal.Count)"
        if ($pcDiag.Count -gt 0 -and $lapDiag.Count -gt 0) { Show-Pairs -PcLine $pcDiag[-1] -LapLine $lapDiag[-1] }
        $expanded = @($pcText -split "`n")
        for ($i = 0; $i -lt $expanded.Count; $i++) {
            if ($expanded[$i] -match 'LeaseExpired') {
                $from = [Math]::Max(0, $i - 3); $to = [Math]::Min($expanded.Count - 1, $i + 3)
                foreach ($j in $from..$to) { Write-Host "PC_EXPIRY_CONTEXT[$j] $($expanded[$j])" }
            }
        }
    }
} finally {
    try {
        if ((Get-MdfStatus -Exe $core) -notmatch 'focus=Local') { Send-Emergency; [void](Wait-Match 'focus=Local' 20 'final_local') }
    } catch { Write-Host "RESET_FAILED=$_" }
    Write-Host ('RUN_FINISH=' + [DateTime]::UtcNow.ToString('o') + ' ' + (Get-MdfStatus -Exe $core))
}
