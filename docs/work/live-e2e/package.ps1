# package.ps1 — build the trial archives of one commit for the PC and the laptop.
#
# One executable since b23335a: `cargo build --release --workspace` produces
# target/release/bifrost.exe, and the Linux archive is built in a throwaway
# lws-ci-linux-deps:rust1.99 container (the host has no Linux toolchain) so both carry a binary built
# from the same commit. Each archive holds bifrost(.exe), README_RU.txt,
# THIRD_PARTY_NOTICES.md and config.example.toml; the contents mirror release.yml —
# the Windows zip adds apps/bifrost/assets/icon.ico, icon.svg and icon-256.png,
# the Linux tar adds packaging/bifrost.desktop plus share/applications and
# share/icons/hicolor (scalable SVG and the 16..256 PNGs). Names are
# bifrost-<shortsha>-windows-x86_64.zip / -linux-x86_64.tar.gz.
#
# target/trial keeps one generation: the previous archives move into a fresh
# trial-backup-<stamp> and older backups are folded into it (their SHA256SUMS.txt is
# renamed side by side; it is the only hash record of those archives).
#
# Usage:
#   ./package.ps1                 # package HEAD (short sha must be 7 hex)
#   ./package.ps1 -Commit b23335a
#   ./package.ps1 -PlanOnly
[CmdletBinding()]
param(
    [string]$Commit,
    [string]$ArchiveVersion,
    [string]$EvidenceDir,
    [switch]$PlanOnly
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/common.ps1"

$root = Get-LwsRoot
$head = (& git -C $root rev-parse --short=7 HEAD).Trim()
if (-not $Commit) { $Commit = $head }
if ($Commit -notmatch '^[0-9a-f]{7}$') { throw "-Commit must be a 7-hex short sha, got '$Commit'" }
if ($Commit -ne $head) { throw "refusing to package ${Commit}: HEAD is $head; check out the commit first" }
$archiveId = if ($ArchiveVersion) { $ArchiveVersion } else { $Commit }
if ($archiveId -notmatch '^[0-9A-Za-z][0-9A-Za-z.-]*$') { throw 'invalid archive version' }
if ($PlanOnly) {
    Write-PlanHeader 'package.ps1'
    Write-Host "commit:     $Commit"
    Write-Host "windows:    cargo build --release --workspace -> target/trial/bifrost-$archiveId-windows-x86_64.zip"
    Write-Host "linux:      docker run --rm lws-ci-linux-deps:rust1.99 cargo build --release --workspace -> target/trial/bifrost-$archiveId-linux-x86_64.tar.gz"
    Write-Host "zip:        bifrost.exe, README_RU.txt, THIRD_PARTY_NOTICES.md, config.example.toml, icon.ico, icon.svg, icon-256.png"
    Write-Host "tarball:    bifrost, README_RU.txt, THIRD_PARTY_NOTICES.md, config.example.toml, bifrost.desktop, share/applications, share/icons/hicolor"
    Write-Host 'writes:     target/trial/SHA256SUMS.txt'
    return
}

$evidence = New-EvidenceDir -EvidenceDir $EvidenceDir
Write-Host "EVIDENCE=$evidence"

# One backup generation only.
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$trial = Join-Path $root 'target/trial'
$backup = Join-Path $root "target/trial-backup-$stamp"
New-Item -ItemType Directory -Force -Path $backup | Out-Null
Get-ChildItem -LiteralPath $trial -File -ErrorAction SilentlyContinue | Move-Item -Destination $backup
Get-ChildItem -LiteralPath (Join-Path $root 'target') -Directory -Filter 'trial-backup-*' |
    Where-Object { $_.FullName -ne (Resolve-Path -LiteralPath $backup).Path } | ForEach-Object {
        $older = $_
        Get-ChildItem -LiteralPath $older.FullName -File | ForEach-Object {
            $target = Join-Path (Resolve-Path -LiteralPath $backup).Path $_.Name
            if (Test-Path -LiteralPath $target) {
                $target = Join-Path (Resolve-Path -LiteralPath $backup).Path "$($_.BaseName)-$($older.Name -replace '^trial-backup-','').$($_.Extension.TrimStart('.'))"
            }
            Move-Item -LiteralPath $_.FullName -Destination $target
        }
        Remove-Item -LiteralPath $older.FullName -Recurse -Force
    }
Write-Host "TRIAL_BACKUP=$(Resolve-Path -LiteralPath $backup).Path"

# --- Windows -----------------------------------------------------------------

$env:CARGO_BUILD_JOBS = '2'
Push-Location $root
try {
    cargo build --release --workspace
    if ($LASTEXITCODE) { throw 'Windows release build failed' }
} finally { Pop-Location }

$stage = Join-Path $trial "stage-windows-$Commit"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
try {
    Copy-Item (Join-Path $root 'target/release/bifrost.exe') $stage
    Copy-Item (Join-Path $root 'packaging/README_RU.txt') $stage
    Copy-Item (Join-Path $root 'licenses/THIRD_PARTY_NOTICES.md') $stage
    Copy-Item (Join-Path $root 'crates/config/examples-data/config.example.toml') $stage
    foreach ($asset in @('icon.ico', 'icon.svg', 'icon-256.png')) {
        Copy-Item (Join-Path $root "apps/bifrost/assets/$asset") $stage
    }
    Compress-Archive -Path "$stage/*" -DestinationPath (Join-Path $trial "bifrost-$archiveId-windows-x86_64.zip") -Force
} finally { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
if (-not (Test-Path -LiteralPath (Join-Path $trial "bifrost-$archiveId-windows-x86_64.zip"))) { throw 'Windows archive missing' }

# --- Linux (throwaway container) ---------------------------------------------

$linux = @'
set -eu
cargo build --release --workspace
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
install -m 0755 "$CARGO_TARGET_DIR/release/bifrost" "$stage/"
install -m 0644 packaging/README_RU.txt licenses/THIRD_PARTY_NOTICES.md crates/config/examples-data/config.example.toml "$stage/"
install -m 0644 packaging/bifrost.desktop "$stage/"
mkdir -p "$stage/share/applications" "$stage/share/icons/hicolor/scalable/apps"
install -m 0644 packaging/bifrost.desktop "$stage/share/applications/"
install -m 0644 packaging/bifrost.svg "$stage/share/icons/hicolor/scalable/apps/bifrost.svg"
for size in 16 20 24 32 40 48 64 256; do
  mkdir -p "$stage/share/icons/hicolor/${size}x${size}/apps"
  install -m 0644 "apps/bifrost/assets/icon-${size}.png" "$stage/share/icons/hicolor/${size}x${size}/apps/bifrost.png"
done
tar -czf "target/trial/bifrost-COMMIT-linux-x86_64.tar.gz" -C "$stage" bifrost README_RU.txt THIRD_PARTY_NOTICES.md config.example.toml bifrost.desktop share
tar -tvzf "target/trial/bifrost-COMMIT-linux-x86_64.tar.gz"
'@ -replace 'COMMIT', $archiveId
$volume = $root.Replace('\', '/')
docker run --rm --name "lws-package-$Commit" -v "${volume}:/work" -w /work `
    -e CARGO_BUILD_JOBS=2 -e CARGO_TARGET_DIR=/work/target/docker -e RUSTUP_TOOLCHAIN=1.99.0 lws-ci-linux-deps:rust1.99 bash -c ("export PATH=/usr/local/cargo/bin:`$PATH; " + $linux.Replace("`r", ''))
if ($LASTEXITCODE) { throw 'Linux release/package failed' }

$zip = "bifrost-$archiveId-windows-x86_64.zip"
$tarball = "bifrost-$archiveId-linux-x86_64.tar.gz"
if (-not (Test-Path -LiteralPath (Join-Path $trial $tarball))) { throw 'Linux archive missing' }
$sums = @(
    "$((Get-FileHash -LiteralPath (Join-Path $trial $zip) -Algorithm SHA256).Hash.ToLowerInvariant())  $zip",
    "$((Get-FileHash -LiteralPath (Join-Path $trial $tarball) -Algorithm SHA256).Hash.ToLowerInvariant())  $tarball"
)
Set-Content -LiteralPath (Join-Path $trial 'SHA256SUMS.txt') -Value $sums
Get-Content -LiteralPath (Join-Path $trial 'SHA256SUMS.txt') | Tee-Object -FilePath (Join-Path $evidence 'SHA256SUMS.txt')
Write-Host "PACKAGED=$Commit"
