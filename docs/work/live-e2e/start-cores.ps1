# start-cores.ps1 — start the service on the PC and on the laptop, and verify both answer.
#
# The core is `bifrost run` (the window starts the same service the same way; the
# agent is spawned by the core and is not started by hand). With -Dev the PC core runs
# `bifrost run --dev-capture-injected` and LWS_LOG=debug so the click/edge smoke can
# capture and see externally injected input; the normal end state is restored by
# finalize.ps1. The laptop core runs under a systemd user unit so it lives in the real KDE
# Wayland session.
#
# Usage:
#   ./start-cores.ps1
#   ./start-cores.ps1 -Dev
#   ./start-cores.ps1 -PlanOnly
[CmdletBinding()]
param(
    [string]$Laptop = 'lws-laptop',
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$RemoteInstall = '$HOME/lan-workspace',
    [string]$EvidenceDir,
    [switch]$Dev,
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"

$exe = Join-Path $Install 'bifrost.exe'
$arguments = @('run')
if ($Dev) { $arguments += '--dev-capture-injected' }

if ($PlanOnly) {
    Write-PlanHeader 'start-cores.ps1'
    Write-Host "PC:     $exe $($arguments -join ' ')$(if ($Dev) { '  (with LWS_LOG=debug)' })"
    Write-Host "laptop: systemd-run --user --unit=lws-core-live-test --working-directory=$RemoteInstall $RemoteInstall/bifrost run"
    Write-Host 'then:   both sides must answer `status` with focus=Local'
    return
}

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

$running = @(Get-InstalledProcesses -Install $Install | Where-Object { $_.ProcessName -eq 'bifrost' })
if ($running.Count -gt 0) {
    throw "bifrost is already running from the install (PIDs $($running.Id -join ',')); stop it (deploy.ps1 does) before this helper."
}
if (-not (Test-Path -LiteralPath $exe)) { throw "missing $exe; run deploy.ps1 first" }

if ($Dev) { $env:LWS_LOG = 'debug' }
try {
    $process = Start-Process -FilePath $exe -ArgumentList $arguments -PassThru -WindowStyle Hidden
} finally {
    if ($Dev) { Remove-Item Env:LWS_LOG -ErrorAction SilentlyContinue }
}
Write-Host "PC_CORE_PID=$($process.Id) dev=$([bool]$Dev)"
Write-Host "PC_STATUS_START=$(Wait-MdfFocus -Exe $exe -Focus 'Local' -TimeoutSeconds 30)"

$remote = @"
set -eu
# `pgrep -f 'bifrost run'` would match this very shell (its command line
# contains the pattern), so select by process name and look at the argv instead.
if ps -o args= -C bifrost 2>/dev/null | grep -qE '/bifrost run( |$)'; then
  echo 'Laptop bifrost run is already running; refusing to start a duplicate.' >&2
  exit 2
fi
uid=`$(id -u)
env=`$(systemctl --user show-environment)
printf '%s\n' "`$env" | grep -qx 'WAYLAND_DISPLAY=wayland-0'
printf '%s\n' "`$env" | grep -qx 'XDG_CURRENT_DESKTOP=KDE'
systemd-run --user --unit=lws-core-live-test --collect \
  --working-directory="$RemoteInstall" \
  --setenv="XDG_RUNTIME_DIR=/run/user/`$uid" \
  --setenv="DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/`$uid/bus" \
  --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=DISPLAY=:1 \
  --setenv=XDG_SESSION_TYPE=wayland --setenv=XDG_CURRENT_DESKTOP=KDE \
  "$RemoteInstall/bifrost" run
sleep 2
systemctl --user --no-pager --full status lws-core-live-test
"@
& ssh $Laptop ($remote.Replace("`r", ''))
if ($LASTEXITCODE -ne 0) { throw 'Laptop core start failed; verify its user-session environment and service state.' }

Start-Sleep -Seconds 3
$pcStatus = Get-MdfStatus -Exe $exe
$laptopStatus = (& ssh $Laptop "$RemoteInstall/bifrost status" | Out-String)
if ($LASTEXITCODE -ne 0) { throw "laptop core did not answer status: $laptopStatus" }
Set-Content -LiteralPath (Join-Path $evidence 'start-cores.txt') -Value @("PC_CORE_PID=$($process.Id)", "PC $pcStatus", "LAPTOP $($laptopStatus.Trim())")
Write-Host "PC_STATUS=$pcStatus"
Write-Host "LAPTOP_STATUS=$($laptopStatus.Trim())"
