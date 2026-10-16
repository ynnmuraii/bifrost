# common.ps1 — shared helpers for the live-e2e scripts. Dot-source it at the top of a
# script:   . "$PSScriptRoot/common.ps1"
#
# These scripts drive two real machines: the PC (this Windows host, install dir
# C:\Users\tihoo\Desktop\flow) and the laptop (`ssh lws-laptop`, KDE Wayland, install
# dir ~/lan-workspace). Since b23335a the product is ONE executable, `bifrost`
# (`bifrost.exe`): no args opens the window, `run` starts the service, `agent …`
# is internal, and every other argument is the core CLI (`status`, `switch`, `claim`,
# `peers`, `layout …`).
#
# On Windows the executable is a GUI-subsystem program, so a direct
# `& bifrost.exe status` does not wait and leaves $LASTEXITCODE empty (see
# packaging/README_RU.txt §12). Every call therefore goes through Invoke-Mdf, which uses
# Start-Process -Wait with redirected output and returns the text and the exit code.
#
# Nothing here contacts a machine on its own; the callers decide that.

$script:LwsRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../../..')).Path

function Get-LwsRoot { return $script:LwsRoot }

# --- trial archives ---------------------------------------------------------

# The newest trial generation lives in target/trial: SHA256SUMS.txt lists the two
# archives of that generation, and the archives themselves are the ones the deploy
# installs. -Commit picks a specific short sha; without it the newest Windows archive
# named in SHA256SUMS.txt wins (falling back to the newest file there).
function Get-TrialArchives {
    param([string]$Commit)
    $dir = Join-Path (Get-LwsRoot) 'target/trial'
    $sums = Join-Path $dir 'SHA256SUMS.txt'
    $map = @{}
    if (Test-Path -LiteralPath $sums) {
        foreach ($line in Get-Content -LiteralPath $sums) {
            if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(\S+)\s*$') { $map[$Matches[2]] = $Matches[1].ToLowerInvariant() }
        }
    }
    if ($Commit) {
        $rev = $Commit
    } else {
        $candidates = @()
        foreach ($name in $map.Keys) {
            if ($name -match '^bifrost-(.+)-windows-x86_64\.zip$') { $candidates += (Join-Path $dir $name) }
        }
        if ($candidates.Count -eq 0) {
            $candidates = @(Get-ChildItem -LiteralPath $dir -Filter 'bifrost-*-windows-x86_64.zip' -File -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName)
        }
        if ($candidates.Count -eq 0) { throw "no Windows trial archive under $dir; run package.ps1 first" }
        $newest = $candidates | Sort-Object { (Get-Item -LiteralPath $_).LastWriteTimeUtc } -Descending | Select-Object -First 1
        $rev = [regex]::Match((Split-Path -Leaf $newest), '^bifrost-(.+)-windows-x86_64\.zip$').Groups[1].Value
    }
    $win = Join-Path $dir "bifrost-$rev-windows-x86_64.zip"
    $linux = Join-Path $dir "bifrost-$rev-linux-x86_64.tar.gz"
    if (-not (Test-Path -LiteralPath $win)) { throw "missing Windows archive: $win" }
    if (-not (Test-Path -LiteralPath $linux)) { throw "missing Linux archive: $linux" }
    $winName = Split-Path -Leaf $win
    $linuxName = Split-Path -Leaf $linux
    $winSha = if ($map.ContainsKey($winName)) { $map[$winName] } else { (Get-FileHash -LiteralPath $win -Algorithm SHA256).Hash.ToLowerInvariant() }
    $linuxSha = if ($map.ContainsKey($linuxName)) { $map[$linuxName] } else { (Get-FileHash -LiteralPath $linux -Algorithm SHA256).Hash.ToLowerInvariant() }
    if ((Get-FileHash -LiteralPath $win -Algorithm SHA256).Hash.ToLowerInvariant() -ne $winSha) { throw "Windows archive SHA mismatch: $win" }
    if ((Get-FileHash -LiteralPath $linux -Algorithm SHA256).Hash.ToLowerInvariant() -ne $linuxSha) { throw "Linux archive SHA mismatch: $linux" }
    [pscustomobject]@{
        Rev = $rev
        WindowsArchive = $win
        LinuxArchive = $linux
        WindowsSha = $winSha
        LinuxSha = $linuxSha
    }
}

# --- evidence ---------------------------------------------------------------

# Every run writes under target/live-e2e/runs/<timestamp>/ (evidence never leaves
# target/). Pass -EvidenceDir to reuse the directory of an earlier step of the same run.
function New-EvidenceDir {
    param([string]$EvidenceDir)
    if (-not $EvidenceDir) {
        $EvidenceDir = Join-Path (Get-LwsRoot) ("target/live-e2e/runs/" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
    } elseif (-not [System.IO.Path]::IsPathRooted($EvidenceDir)) {
        $EvidenceDir = Join-Path (Get-LwsRoot) $EvidenceDir
    }
    New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null
    return (Resolve-Path -LiteralPath $EvidenceDir).Path
}

# --- calling bifrost ---------------------------------------------------

# Start-Process -Wait with the output redirected to files: the only way to get both the
# text and the exit code out of a GUI-subsystem binary from PowerShell.
function Invoke-Mdf {
    param([string]$Exe, [string[]]$Arguments)
    $out = New-TemporaryFile
    $err = New-TemporaryFile
    try {
        $process = Start-Process -FilePath $Exe -ArgumentList $Arguments -Wait -NoNewWindow -PassThru `
            -RedirectStandardOutput $out.FullName -RedirectStandardError $err.FullName
        $text = [string](Get-Content -LiteralPath $out.FullName -Raw -ErrorAction SilentlyContinue)
        $text += [string](Get-Content -LiteralPath $err.FullName -Raw -ErrorAction SilentlyContinue)
        return [pscustomobject]@{ Code = $process.ExitCode; Text = $text }
    } finally {
        Remove-Item -LiteralPath $out.FullName, $err.FullName -Force -ErrorAction SilentlyContinue
    }
}

function Get-MdfStatus {
    param([string]$Exe)
    $call = Invoke-Mdf -Exe $Exe -Arguments @('status')
    if ($call.Code -ne 0) { throw "core status failed (rc=$($call.Code)): $($call.Text)" }
    $match = [regex]::Match($call.Text, '(?m)^OK focus=.*$')
    if (-not $match.Success) { throw "core status omitted its status line: $($call.Text)" }
    return $match.Value
}

function Wait-MdfFocus {
    param([string]$Exe, [string]$Focus, [int]$TimeoutSeconds = 20)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $status = ''
    do {
        $status = Get-MdfStatus -Exe $Exe
        if ($status -match "focus=$Focus\b") { return $status }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "timed out waiting for focus=$Focus; last status: $status"
}

# --- installed processes -----------------------------------------------------

# Another worker's fault-injection tests run binaries from their own target dir under
# the same process names; only processes launched from the install dir are ours to
# stop. `Path` can be refused for processes this user cannot open, so it is read
# defensively.
function Get-InstalledProcesses {
    param([string]$Install)
    $prefix = (($Install.TrimEnd('\', '/')) -replace '/', '\') + '\'
    $found = @()
    foreach ($name in @('bifrost', 'multideskflow', 'lws-core', 'lws-agent', 'lws-gui')) {
        foreach ($process in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
            $path = $null
            try { $path = $process.Path } catch { $path = $null }
            if ($path -and (($path -replace '/', '\').StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase))) { $found += $process }
        }
    }
    return $found
}

function Stop-InstalledProcesses {
    param([string]$Install)
    $stopped = @(Get-InstalledProcesses -Install $Install)
    foreach ($process in $stopped) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    return $stopped
}

# The leftover executables of the pre-b23335a three-file layout. The one executable
# ignores them, so they are removed on every deploy.
function Remove-LegacyWindowsBinaries {
    param([string]$Install)
    foreach ($name in @('multideskflow.exe', 'lws-core.exe', 'lws-agent.exe', 'lws-gui.exe')) {
        Remove-Item -LiteralPath (Join-Path $Install $name) -Force -ErrorAction SilentlyContinue
    }
}

# Migrate only this install's startup commands; unrelated applications stay untouched.
function Update-BifrostWindowsAutostart {
    param([string]$Install)
    $oldExe = [IO.Path]::GetFullPath((Join-Path $Install 'multideskflow.exe'))
    $newExe = [IO.Path]::GetFullPath((Join-Path $Install 'bifrost.exe'))
    $run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    if (Test-Path $run) {
        $key = Get-Item $run
        foreach ($name in $key.GetValueNames()) {
            $value = $key.GetValue($name)
            if ($value -is [string] -and $value.IndexOf($oldExe, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                $updated = $value -replace [regex]::Escape($oldExe), $newExe
                New-ItemProperty -Path $run -Name 'Bifrost' -Value $updated -PropertyType String -Force | Out-Null
                if ($name -ne 'Bifrost') { Remove-ItemProperty -Path $run -Name $name }
            }
        }
    }
    foreach ($task in @(Get-ScheduledTask -ErrorAction SilentlyContinue)) {
        if (@($task.Actions | Where-Object { $_.Execute -and $_.Execute.Trim('"') -ieq $oldExe }).Count -eq 0) { continue }
        $xml = Export-ScheduledTask -TaskName $task.TaskName -TaskPath $task.TaskPath
        $xml = $xml -replace [regex]::Escape($oldExe), $newExe
        $name = $task.TaskName -replace '(?i)multideskflow', 'Bifrost'
        Register-ScheduledTask -TaskName $name -TaskPath $task.TaskPath -Xml $xml -Force | Out-Null
        if ($name -ne $task.TaskName) { Unregister-ScheduledTask -TaskName $task.TaskName -TaskPath $task.TaskPath -Confirm:$false }
    }
}

# --- input simulation --------------------------------------------------------

$script:LiveInputSource = @'
using System;
using System.Runtime.InteropServices;
public static class LiveInput {
    [StructLayout(LayoutKind.Sequential)] public struct MouseInput {
        public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo;
    }
    [StructLayout(LayoutKind.Sequential)] public struct KeyboardInput {
        public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo;
    }
    [StructLayout(LayoutKind.Explicit)] public struct InputUnion {
        [FieldOffset(0)] public MouseInput mouse;
        [FieldOffset(0)] public KeyboardInput keyboard;
    }
    [StructLayout(LayoutKind.Sequential)] public struct Input { public uint type; public InputUnion data; }
    [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint count, Input[] inputs, int size);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
    public static void Mouse(int dx, int dy, uint flags) {
        Input input = new Input(); input.type = 0;
        input.data.mouse = new MouseInput { dx=dx, dy=dy, dwFlags=flags, dwExtraInfo=IntPtr.Zero };
        if (SendInput(1, new Input[]{input}, Marshal.SizeOf(typeof(Input))) != 1) throw new InvalidOperationException("SendInput mouse failed: " + Marshal.GetLastWin32Error());
    }
    public static void ScanKey(ushort scan, uint flags) {
        Input input = new Input(); input.type = 1;
        input.data.keyboard = new KeyboardInput { wVk=0, wScan=scan, dwFlags=flags | 0x0008, dwExtraInfo=IntPtr.Zero };
        if (SendInput(1, new Input[]{input}, Marshal.SizeOf(typeof(Input))) != 1) throw new InvalidOperationException("SendInput keyboard failed: " + Marshal.GetLastWin32Error());
    }
}
'@

function Initialize-LiveInput {
    if ([System.Management.Automation.PSTypeName]'LiveInput'.Type) { return }
    Add-Type -TypeDefinition $script:LiveInputSource
}

function Send-AbsolutePointer {
    param([int]$X, [int]$Y)
    $left = [LiveInput]::GetSystemMetrics(76); $top = [LiveInput]::GetSystemMetrics(77)
    $width = [LiveInput]::GetSystemMetrics(78); $height = [LiveInput]::GetSystemMetrics(79)
    if ($width -le 1 -or $height -le 1) { throw 'Windows reported an invalid virtual desktop.' }
    $nx = [int][Math]::Round(($X - $left) * 65535.0 / [Math]::Max(1, $width - 1))
    $ny = [int][Math]::Round(($Y - $top) * 65535.0 / [Math]::Max(1, $height - 1))
    [LiveInput]::Mouse($nx, $ny, 0x0001 -bor 0x8000 -bor 0x4000)
}

# Push the pointer out of one vertical edge: place it near the edge, walk the last few
# pixels, then request extra relative motion past the physical boundary.
function Send-EdgePush {
    param([string]$Direction)
    $left = [LiveInput]::GetSystemMetrics(76); $top = [LiveInput]::GetSystemMetrics(77)
    $width = [LiveInput]::GetSystemMetrics(78); $height = [LiveInput]::GetSystemMetrics(79)
    if ($width -le 1 -or $height -le 1) { throw 'Windows reported an invalid virtual desktop.' }
    $y = $top + [int][Math]::Floor(($height - 1) / 2)
    $step = [int]$(if ($Direction -eq 'left') { -48 } else { 48 })
    $x = [int]$(if ($Direction -eq 'left') { $left } else { $left + $width - 1 })
    Send-AbsolutePointer -X $x -Y $y
    Start-Sleep -Milliseconds 150
    1..5 | ForEach-Object { [LiveInput]::Mouse($step, 0, 0x0001); Start-Sleep -Milliseconds 80 }
    [LiveInput]::Mouse($step * 5, 0, 0x0001)
}

# One left click: press, 50 ms, release.
function Send-Click {
    [LiveInput]::Mouse(0, 0, 0x0002); Start-Sleep -Milliseconds 50
    [LiveInput]::Mouse(0, 0, 0x0004)
}

function Send-Key {
    param([uint16]$Scan, [switch]$Up)
    [LiveInput]::ScanKey($Scan, $(if ($Up) { [uint32]0x0002 } else { [uint32]0 }))
}

function Send-Chord {
    param([uint16[]]$Keys)
    foreach ($key in $Keys) { Send-Key -Scan $key }
    for ($i = $Keys.Length - 1; $i -ge 0; $i--) { Send-Key -Scan $Keys[$i] -Up }
}

# Ctrl+Alt+Shift+F12 — the emergency combination that takes input back to local.
function Send-Emergency { Send-Chord -Keys @([uint16]0x1D, [uint16]0x38, [uint16]0x2A, [uint16]0x58) }

function Send-Text {
    param([string]$Text)
    $letters = @{
        A=0x1E; B=0x30; C=0x2E; D=0x20; E=0x12; F=0x21; G=0x22; H=0x23;
        I=0x17; J=0x24; K=0x25; L=0x26; M=0x32; N=0x31; O=0x18; P=0x19;
        Q=0x10; R=0x13; S=0x1F; T=0x14; U=0x16; V=0x2F; W=0x11; X=0x2D;
        Y=0x15; Z=0x2C
    }
    $digits = @{ '1'=0x02; '2'=0x03; '3'=0x04; '4'=0x05; '5'=0x06; '6'=0x07; '7'=0x08; '8'=0x09; '9'=0x0A; '0'=0x0B }
    foreach ($char in $Text.ToCharArray()) {
        $scan = [uint16]0; $shift = $false
        if ($char -ge 'A' -and $char -le 'Z') { $scan = [uint16]$letters[[string]$char]; $shift = $true }
        elseif ($char -ge 'a' -and $char -le 'z') { $scan = [uint16]$letters[([string]$char).ToUpperInvariant()] }
        elseif ($digits.ContainsKey([string]$char)) { $scan = [uint16]$digits[[string]$char] }
        elseif ($char -eq '-') { $scan = [uint16]0x0C }
        else { throw "unsupported test character: $char" }
        if ($shift) { Send-Key -Scan 0x2A }
        Send-Key -Scan $scan
        Send-Key -Scan $scan -Up
        if ($shift) { Send-Key -Scan 0x2A -Up }
    }
}

# --- log deltas --------------------------------------------------------------

function Read-FileDelta {
    param([string]$Path, [long]$Offset)
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    if ((Get-Item -LiteralPath $Path).Length -lt $Offset) { $Offset = 0 }
    $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
        $stream.Seek($Offset, 'Begin') | Out-Null
        $reader = New-Object System.IO.StreamReader($stream)
        return $reader.ReadToEnd()
    } finally { $stream.Dispose() }
}

function Get-FileSize {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { return [int64](Get-Item -LiteralPath $Path).Length }
    return [int64]0
}

function Get-LaptopFileSize {
    param([string]$Laptop, [string]$Path)
    $size = & ssh $Laptop "if [ -f '$Path' ]; then stat -c %s '$Path'; else echo 0; fi"
    if ($LASTEXITCODE) { throw "laptop log size failed: $Path" }
    return [int64](($size | Out-String).Trim())
}

function Get-LaptopFileDelta {
    param([string]$Laptop, [string]$Path, [int64]$Offset)
    return (& ssh $Laptop "if [ -f '$Path' ]; then size=`$(stat -c %s '$Path'); if [ `$size -lt $Offset ]; then offset=0; else offset=$Offset; fi; tail -c +`$((offset + 1)) '$Path'; fi" | Out-String)
}

function Select-Matching {
    param([string]$Text, [string]$Pattern)
    return @($Text -split "`n" | Where-Object { $_ -match $Pattern })
}

function Show-Lines {
    param([string]$Name, [string[]]$Lines, [int]$Head = 2, [int]$Tail = 2)
    Write-Host "$Name COUNT=$($Lines.Count)"
    $indexes = @()
    if ($Lines.Count -le ($Head + $Tail)) { $indexes = 0..([Math]::Max(0, $Lines.Count - 1)) }
    elseif ($Lines.Count -gt 0) { $indexes = @(0..($Head - 1)) + @(($Lines.Count - $Tail)..($Lines.Count - 1)) }
    foreach ($index in $indexes) { if ($index -ge 0 -and $index -lt $Lines.Count) { Write-Host "$Name[$index] $($Lines[$index])" } }
}

# --- plan mode ---------------------------------------------------------------

function Write-PlanHeader {
    param([string]$Name)
    Write-Host "PLAN $Name — no machine will be contacted."
}
