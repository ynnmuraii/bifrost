# deploy.ps1 — install one trial generation on the PC and the laptop and start nothing.
#
# The archives come from package.ps1. Their SHA256 is verified before anything on either
# machine is touched, and again on the laptop after the copy. The current install dir is
# moved aside to exactly one *.old-<stamp> copy (config.toml, trust.toml and
# portal-restore.toml are carried over untouched); older backups are removed. The
# leftover pre-b23335a executables (lws-core/lws-agent/lws-gui) are deleted.
#
# start-cores.ps1 starts the services afterwards, finalize.ps1 owns the end state.
#
# Usage:
#   ./deploy.ps1                                  # newest archive in target/trial
#   ./deploy.ps1 -Commit b23335a
#   ./deploy.ps1 -Archive target/trial/bifrost-b23335a-windows-x86_64.zip
#   ./deploy.ps1 -PlanOnly
[CmdletBinding()]
param(
    [string]$Commit,
    [string]$Archive,
    [string]$LinuxArchive,
    [string]$Laptop = 'lws-laptop',
    [string]$Install = 'C:/Users/tihoo/Desktop/flow',
    [string]$RemoteInstall = '$HOME/lan-workspace',
    [string]$EvidenceDir,
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"

if ($Archive) {
    $win = (Resolve-Path -LiteralPath $Archive).Path
    if (-not $LinuxArchive) { $LinuxArchive = $win -replace 'windows-x86_64\.zip$', 'linux-x86_64.tar.gz' }
    $linux = (Resolve-Path -LiteralPath $LinuxArchive).Path
    $rev = [regex]::Match((Split-Path -Leaf $win), '^bifrost-(.+)-windows-x86_64\.zip$').Groups[1].Value
    if (-not $rev) { throw "cannot read the commit from $win" }
    $winSha = (Get-FileHash -LiteralPath $win -Algorithm SHA256).Hash.ToLowerInvariant()
    $linuxSha = (Get-FileHash -LiteralPath $linux -Algorithm SHA256).Hash.ToLowerInvariant()
} else {
    $trial = Get-TrialArchives -Commit $Commit
    $rev = $trial.Rev
    $win = $trial.WindowsArchive
    $linux = $trial.LinuxArchive
    $winSha = $trial.WindowsSha
    $linuxSha = $trial.LinuxSha
}

if ($PlanOnly) {
    Write-PlanHeader 'deploy.ps1'
    Write-Host "commit:      $rev"
    Write-Host "windows zip: $win  sha256=$winSha"
    Write-Host "linux tar:   $linux  sha256=$linuxSha"
    Write-Host "PC:          extract to a staging dir, verify bifrost.exe, stop installed"
    Write-Host "             processes, $Install -> $Install.old-<stamp>, stage -> $Install,"
    Write-Host "             restore config/trust/portal-restore, delete lws-*.exe, keep one backup"
    Write-Host "laptop:      scp to /tmp, sha256sum -c, stop units and bifrost/lws-*,"
    Write-Host "             $RemoteInstall -> backup, extract, restore configs, chmod 0755, keep one backup"
    return
}

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"
Write-Host "DEPLOY_REV=$rev"
Write-Host "WINDOWS_SHA=$winSha"
Write-Host "LINUX_SHA=$linuxSha"

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

# --- Windows -----------------------------------------------------------------

# Extract and verify first: a bad archive must not cost the current install.
$stage = "$Install.new-$stamp"
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
Expand-Archive -LiteralPath $win -DestinationPath $stage
if (-not (Test-Path -LiteralPath (Join-Path $stage 'bifrost.exe'))) {
    Remove-Item -LiteralPath $stage -Recurse -Force
    throw "$win does not contain bifrost.exe; it is a pre-b23335a archive — re-package"
}
Write-Host "STAGED_EXE_SHA=$((Get-FileHash -LiteralPath (Join-Path $stage 'bifrost.exe') -Algorithm SHA256).Hash.ToLowerInvariant())"

$stopped = Stop-InstalledProcesses -Install $Install
Write-Host "PC_STOPPED=$($stopped.Count)"
Start-Sleep -Seconds 1

$backup = "$Install.old-$stamp"
if (Test-Path -LiteralPath $Install) { Move-Item -LiteralPath $Install -Destination $backup }
Move-Item -LiteralPath $stage -Destination $Install
foreach ($name in @('config.toml', 'trust.toml', 'portal-restore.toml')) {
    if (Test-Path -LiteralPath (Join-Path $backup $name)) { Copy-Item -LiteralPath (Join-Path $backup $name) -Destination (Join-Path $Install $name) }
}
Remove-LegacyWindowsBinaries -Install $Install
Update-BifrostWindowsAutostart -Install $Install
$parent = Split-Path -Parent $Install
$leaf = Split-Path -Leaf $Install
Get-ChildItem -LiteralPath $parent -Directory -Filter "$leaf.old-*" -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -ne (Resolve-Path -LiteralPath $backup -ErrorAction SilentlyContinue).Path } |
    Remove-Item -Recurse -Force
Write-Host "PC_BACKUP=$backup"
Write-Host "PC_EXE_SHA=$((Get-FileHash -LiteralPath (Join-Path $Install 'bifrost.exe') -Algorithm SHA256).Hash.ToLowerInvariant())"

# --- Laptop ------------------------------------------------------------------

$remoteArchive = "/tmp/bifrost-$rev-$stamp.tar.gz"
scp -- $linux "${Laptop}:$remoteArchive"
if ($LASTEXITCODE) { throw 'Linux archive copy failed' }

$remote = @"
set -eu
printf '%s  %s\n' '$linuxSha' '$remoteArchive' | sha256sum -c -
systemctl --user stop app-bifrost@lws1.service lws-core-live-test lws-gui-trial 2>/dev/null || true
pkill -x bifrost || true
pkill -x multideskflow || true
pkill -x lws-gui || true
pkill -x lws-core || true
pkill -x lws-agent || true
sleep 1
install_dir="$RemoteInstall"
backup="`$install_dir.old-$stamp"
if [ -d "`$install_dir" ]; then mv "`$install_dir" "`$backup"; fi
mkdir -p "`$install_dir"
tar -xzf '$remoteArchive' -C "`$install_dir"
for name in config.toml trust.toml portal-restore.toml; do if [ -f "`$backup/`$name" ]; then cp "`$backup/`$name" "`$install_dir/`$name"; fi; done
rm -f "`$install_dir/multideskflow" "`$install_dir/lws-core" "`$install_dir/lws-agent" "`$install_dir/lws-gui"
chmod 0755 "`$install_dir/bifrost"
test -x "`$install_dir/bifrost"
stat -c '%a %n' "`$install_dir/bifrost"
sha256sum "`$install_dir/bifrost"
# Install the new desktop identity; remove the old portal launcher and autostart identity.
applications="`${XDG_DATA_HOME:-`$HOME/.local/share}/applications"
autostart="`${XDG_CONFIG_HOME:-`$HOME/.config}/autostart"
mkdir -p "`$applications"
sed "s|^Exec=.*|Exec=`$install_dir/bifrost|;s|^Icon=.*|Icon=`$install_dir/share/icons/hicolor/scalable/apps/bifrost.svg|" "`$install_dir/bifrost.desktop" > "`$applications/bifrost.desktop"
rm -f "`$applications/multideskflow.desktop"
if [ -f "`$autostart/multideskflow.desktop" ]; then
  sed 's/multideskflow/bifrost/g;s/Name=bifrost/Name=Bifrost/' "`$autostart/multideskflow.desktop" > "`$autostart/bifrost.desktop"
  rm -f "`$autostart/multideskflow.desktop"
fi
for old in "`$install_dir".old-*; do [ -d "`$old" ] || continue; [ "`$old" = "`$backup" ] && continue; rm -rf "`$old"; done
echo LAPTOP_BACKUP="`$backup"
"@
& ssh $Laptop ($remote.Replace("`r", ''))
if ($LASTEXITCODE) { throw 'Laptop deployment failed' }
Write-Host "DEPLOYED_$rev"
