# cross-live.ps1 — end-to-end live crossing: type into a laptop editor over the edge.
#
# Starts the dev core on both machines (start-cores.ps1 -Dev), applies the layout, opens a
# cat-backed editor on the laptop in its KDE Wayland session, crosses the right edge, types a
# marker, verifies the laptop received it, takes a laptop screenshot, holds Remote idle for
# five seconds, sends the emergency chord, and checks both cores' logs for the whole capture
# -> transport -> inject path. Needs a paired, connected Local owner.
#
# Usage:
#   ./cross-live.ps1 -LayoutToml target/live-e2e/layout-live.toml
#   ./cross-live.ps1 -LayoutToml <file> -PlanOnly
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$LayoutToml,
    [string]$Laptop = 'lws-laptop',
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$LaptopLogDir = '/home/ynnmuraii/.local/state/lan-workspace/logs',
    [string]$WindowsLogDir = (Join-Path $env:LOCALAPPDATA 'lan-workspace\logs'),
    [string]$EvidenceDir,
    [string]$Screenshot,
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"
Initialize-LiveInput

$core = Join-Path $Install 'bifrost.exe'
$layout = (Resolve-Path -LiteralPath $LayoutToml).Path
if (-not (Test-Path -LiteralPath $layout -PathType Leaf)) { throw "missing layout TOML: $layout" }
$startScript = Join-Path $PSScriptRoot 'start-cores.ps1'
$layoutScript = Join-Path $PSScriptRoot 'apply-layout.ps1'

if ($PlanOnly) {
    Write-PlanHeader 'cross-live.ps1'
    Write-Host "core:     $core"
    Write-Host "layout:   $layout"
    Write-Host "starts:   start-cores.ps1 -Dev, then apply-layout.ps1 -LayoutToml $layout"
    Write-Host 'sequence: cross the right edge, type into the laptop cat editor, verify it saved,'
    Write-Host '          screenshot the laptop, hold Remote idle 5 s, send Ctrl+Alt+Shift+F12,'
    Write-Host '          then check both cores logs for capture -> transport -> inject.'
    return
}

if (-not (Test-Path -LiteralPath $core)) { throw "missing $core" }

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"
if (-not $Screenshot) { $Screenshot = Join-Path $evidence 'laptop-crossing.png' }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$testText = "LWS-CROSS-$stamp"
$remoteText = "/tmp/lws-cross-live-$stamp.txt"
$remoteScreenshot = "/tmp/lws-cross-live-$stamp.png"
$editorUnit = "lws-cross-editor-$stamp"
$screenshotUnit = "lws-cross-shot-$stamp"

# Snapshot log sizes so old runs cannot satisfy this run's checks.
$beforeWindows = @{}
if (Test-Path -LiteralPath $WindowsLogDir) {
    Get-ChildItem -LiteralPath $WindowsLogDir -Filter '*.log' -File | ForEach-Object { $beforeWindows[$_.FullName] = $_.Length }
}

& $startScript -Laptop $Laptop -Install $Install -Dev
& $layoutScript -LayoutToml $layout -Install $Install

$remoteSetup = @"
set -eu
command -v kitty >/dev/null
command -v spectacle >/dev/null
: > '$remoteText'
uid=`$(id -u)
systemd-run --user --unit='$editorUnit' --collect \
  --working-directory="`$HOME" \
  --setenv="XDG_RUNTIME_DIR=/run/user/`$uid" \
  --setenv="DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/`$uid/bus" \
  --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=DISPLAY=:1 \
  --setenv=XDG_SESSION_TYPE=wayland --setenv=XDG_CURRENT_DESKTOP=KDE \
  kitty --class lws-live-test -e sh -c 'cat > "$remoteText"'
"@
& ssh $Laptop $remoteSetup
if ($LASTEXITCODE -ne 0) { throw 'could not open the laptop editor in its KDE session' }
Start-Sleep -Seconds 3

$remoteSnapshotCommand = @"
set -eu
for f in "$LaptopLogDir/lws-core.log" "$LaptopLogDir/lws-agent.log"; do
  if [ -f "`$f" ]; then printf '%s %s\n' "`$f" "`$(stat -c %s "`$f")"; else printf '%s 0\n' "`$f"; fi
done
"@
$beforeLaptop = @{}
& ssh $Laptop $remoteSnapshotCommand | ForEach-Object {
    if ($_ -match '^(\S+)\s+(\d+)$') { $beforeLaptop[$Matches[1]] = [int64]$Matches[2] }
}
if ($LASTEXITCODE -ne 0) { throw 'could not snapshot laptop logs' }

$initial = Get-MdfStatus -Exe $core
if ($initial -notmatch '^OK focus=Local role=owner .*members=[1-9]\d*') { throw "expected a connected Local owner before crossing; got: $initial" }
Send-EdgePush -Direction 'right'
[void](Wait-MdfFocus -Exe $core -Focus 'Remote' -TimeoutSeconds 20)
Send-Text -Text $testText
Send-Key -Scan 0x1C # Enter flushes the cat-backed line.
Start-Sleep -Milliseconds 800
$saveResult = & ssh $Laptop "set -eu; test `"`$(cat '$remoteText')`" = '$testText'; echo SAVED"
if ($LASTEXITCODE -ne 0 -or ($saveResult -join '') -notmatch 'SAVED') { throw "laptop cat target did not receive the injected text: $($saveResult -join ' ')" }

$remoteShotCommand = @"
set -eu
uid=`$(id -u)
systemd-run --user --wait --collect --unit='$screenshotUnit' \
  --setenv="XDG_RUNTIME_DIR=/run/user/`$uid" \
  --setenv="DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/`$uid/bus" \
  --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=DISPLAY=:1 \
  --setenv=XDG_SESSION_TYPE=wayland --setenv=XDG_CURRENT_DESKTOP=KDE \
  spectacle --background --nonotify --fullscreen --output '$remoteScreenshot'
test -s '$remoteScreenshot'
"@
& ssh $Laptop $remoteShotCommand
if ($LASTEXITCODE -ne 0) { throw 'laptop screenshot capture failed' }
$Screenshot = [IO.Path]::GetFullPath($Screenshot)
$screenDir = Split-Path -Parent $Screenshot
if ($screenDir) { New-Item -ItemType Directory -Path $screenDir -Force | Out-Null }
& scp -- "${Laptop}:$remoteScreenshot" $Screenshot
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $Screenshot -PathType Leaf)) { throw "could not collect laptop screenshot at $Screenshot" }
Send-Chord -Keys @([uint16]0x1D, [uint16]0x20) # Ctrl+D closes the cat-backed terminal after the screenshot.
Start-Sleep -Milliseconds 300

# Remote-mode pointer movement is Raw Input-only, so SendInput cannot automate a natural
# return. Keep the receiver lease idle for five seconds, then verify the emergency release.
Start-Sleep -Seconds 5
$heldStatus = Get-MdfStatus -Exe $core
if ($heldStatus -notmatch '^OK focus=Remote role=owner .*members=[1-9]\d*') { throw "lease did not remain Remote with a peer for five idle seconds; got: $heldStatus" }
Send-Emergency # Ctrl+Alt+Shift+F12
[void](Wait-MdfFocus -Exe $core -Focus 'Local' -TimeoutSeconds 20)

$windowsParts = @()
if (Test-Path -LiteralPath $WindowsLogDir) {
    Get-ChildItem -LiteralPath $WindowsLogDir -Filter '*.log' -File | ForEach-Object {
        $offset = 0
        if ($beforeWindows.ContainsKey($_.FullName)) { $offset = [int64]$beforeWindows[$_.FullName] }
        $windowsParts += (Read-FileDelta -Path $_.FullName -Offset $offset)
    }
}
$windowsDelta = ($windowsParts -join "`n")
$laptopCommands = @('set -eu')
foreach ($entry in $beforeLaptop.GetEnumerator()) {
    $remotePath = $entry.Key.Replace("'", "'\''")
    $laptopCommands += "if [ -f '$remotePath' ]; then tail -c +$([int64]$entry.Value + 1) '$remotePath'; fi"
}
$laptopDelta = (& ssh $Laptop ($laptopCommands -join '; ') | Out-String)
if ($LASTEXITCODE -ne 0) { throw 'could not read appended laptop logs' }
Set-Content -LiteralPath (Join-Path $evidence 'cross-live-windows-delta.log') -Value $windowsDelta
Set-Content -LiteralPath (Join-Path $evidence 'cross-live-laptop-delta.log') -Value $laptopDelta

if ($laptopDelta -notmatch 'focus=Remote' -or $laptopDelta -notmatch 'focus=Local') { throw 'laptop core logs do not show the receiver focus transition and release' }
$finalStatus = Get-MdfStatus -Exe $core
if ($finalStatus -notmatch '^OK focus=Local role=owner') { throw 'final Windows status is not a Local owner' }
if ($finalStatus -notmatch '\bpressed=0\b') { throw 'Windows did not release every pressed input' }
$guis = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'bifrost.exe' -and $_.CommandLine -and $_.CommandLine -notmatch '\b(run|agent)\b' })
if ($guis.Count -eq 0) { throw 'the Windows window (bifrost.exe with no role argument) is not running after the live sequence' }
if ($windowsDelta -notmatch 'dev: capturing externally injected input') { throw 'Windows log lacks the dev-capture startup warning' }
if ($windowsDelta -notmatch 'layout document assembled revision=\d+ links=[1-9]\d*') { throw 'Windows log lacks a usable non-empty layout assembly' }
if ([regex]::Matches($windowsDelta, 'the pointer reached a link edge').Count -lt 1) { throw 'Windows log does not show the right-edge transition' }
if ($windowsDelta -notmatch 'emergency combination: releasing local capture') { throw 'Windows agent log does not show the emergency combination' }
if ($windowsDelta -notmatch 'Windows keyboard/mouse capture started' -or $windowsDelta -notmatch 'Windows capture hook counters' -or $windowsDelta -notmatch 'capture_injected=true' -or $windowsDelta -notmatch 'mapped=[1-9]\d*') { throw 'Windows logs do not prove injected keyboard input reached capture' }
if ($windowsDelta -notmatch 'agent CapturedBatch' -or $windowsDelta -notmatch 'core -> peer.*InputBatch' -or $windowsDelta -notmatch 'transport InputBatch') { throw 'Windows logs do not show the capture-to-transport input path' }
if ($laptopDelta -notmatch 'transport InputBatch' -or $laptopDelta -notmatch 'core -> agent Inject' -or $laptopDelta -notmatch 'agent InjectResult' -or $laptopDelta -notmatch 'EIS injection summary' -or $laptopDelta -notmatch 'EIS device emulation started' -or $laptopDelta -notmatch 'transport InjectAck') { throw 'laptop logs do not show input reaching the agent and EIS injector' }
$remoteState = & ssh $Laptop 'set -eu; systemctl --user is-active lws-core-live-test; ps -o args= -C bifrost | grep -qE "/bifrost run( |$)"; echo CORE_ACTIVE'
if ($LASTEXITCODE -ne 0 -or ($remoteState -join "`n") -notmatch 'CORE_ACTIVE') { throw 'laptop core is not active after the live sequence' }
if ((Get-Item -LiteralPath $Screenshot).Length -le 0) { throw 'collected laptop screenshot is empty' }
Write-Host "PASS: typed and saved '$testText' on the laptop; lease held Remote for five idle seconds; emergency chord restored Local. Screenshot: $Screenshot"
