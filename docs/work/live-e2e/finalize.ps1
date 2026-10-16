# finalize.ps1 — restore the normal end state after a live run and collect the logs.
#
# Copies both machines' logs into the evidence dir, stops the temporary laptop unit, and
# leaves: the PC core running without the dev flag, the PC window open, the laptop core and
# window started detached in the KDE session. Config, trust and portal-restore are never
# touched.
#
# Usage:
#   ./finalize.ps1
#   ./finalize.ps1 -EvidenceDir target/live-e2e/runs/20261014-120000
[CmdletBinding()]
param(
    [string]$Laptop = 'lws-laptop',
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$RemoteInstall = '$HOME/lan-workspace',
    [string]$LaptopLogDir = '/home/ynnmuraii/.local/state/lan-workspace/logs',
    [string]$EvidenceDir
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"

$core = Join-Path $Install 'bifrost.exe'
$gui = Join-Path $Install 'bifrost.exe'
if (-not (Test-Path -LiteralPath $core)) { throw "missing $core" }
$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

# Collect the logs of the run before the services are stopped.
Copy-Item -LiteralPath (Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-core.log') (Join-Path $evidence 'pc-core.log') -Force -ErrorAction SilentlyContinue
Copy-Item -LiteralPath (Join-Path $env:LOCALAPPDATA 'lan-workspace\logs\lws-agent.log') (Join-Path $evidence 'pc-agent.log') -Force -ErrorAction SilentlyContinue
& scp "$Laptop`:$LaptopLogDir/lws-core.log" (Join-Path $evidence 'laptop-core.log')
& scp "$Laptop`:$LaptopLogDir/lws-agent.log" (Join-Path $evidence 'laptop-agent.log')
if ($LASTEXITCODE) { throw 'laptop log copy failed' }

# The laptop core unit goes; the window takes over and starts its own core child.
$remote = @"
set -eu
systemctl --user stop lws-core-live-test lws-gui-trial 2>/dev/null || true
pkill -x bifrost || true
pkill -x lws-gui || true
pkill -x lws-core || true
pkill -x lws-agent || true
sleep 2
uid=`$(id -u)
env=`$(systemctl --user show-environment)
printf '%s\n' "`$env" | grep -qx 'WAYLAND_DISPLAY=wayland-0'
cd "$RemoteInstall"
# The window has no supervision of its own: it only reconnects to a core, so the core is
# started first and the window last, both detached.
XDG_RUNTIME_DIR="/run/user/`$uid" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/`$uid/bus" \
  WAYLAND_DISPLAY=wayland-0 DISPLAY=:1 XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE \
  setsid nohup ./bifrost run >/dev/null 2>&1 < /dev/null &
for attempt in 1 2 3 4 5 6; do
  if ./bifrost status >/dev/null 2>&1; then break; fi
  sleep 2
done
XDG_RUNTIME_DIR="/run/user/`$uid" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/`$uid/bus" \
  WAYLAND_DISPLAY=wayland-0 DISPLAY=:1 XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE \
  setsid nohup ./bifrost >/dev/null 2>&1 < /dev/null &
sleep 5
systemctl --user is-active lws-core-live-test lws-gui-trial || true
pgrep -af bifrost || true
"@
& ssh $Laptop ($remote.Replace("`r", ''))
if ($LASTEXITCODE) { throw 'laptop end state failed' }

$stopped = Stop-InstalledProcesses -Install $Install
Write-Host "PC_STOPPED=$($stopped.Count)"
Start-Sleep -Milliseconds 800
$process = Start-Process -FilePath $core -ArgumentList @('run') -PassThru -WindowStyle Hidden
Write-Host "NORMAL_CORE_PID=$($process.Id)"
Start-Sleep -Seconds 5
Write-Host "PC_STATUS=$(Get-MdfStatus -Exe $core)"
Start-Process -FilePath $gui -WindowStyle Normal
Start-Sleep -Seconds 3
Write-Host "PC_LAYOUT=$((Invoke-Mdf -Exe $core -Arguments @('layout', 'show')).Text.Trim())"

$check = @"
set -eu
printf 'LAPTOP_UNITS='; systemctl --user is-active lws-core-live-test lws-gui-trial || true
"$RemoteInstall/bifrost" status
"$RemoteInstall/bifrost" layout show
"@
& ssh $Laptop ($check.Replace("`r", ''))
if ($LASTEXITCODE) { throw 'final laptop status failed' }
Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'bifrost.exe' } | Select-Object ProcessId, Name, CommandLine | Format-List
