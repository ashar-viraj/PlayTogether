# PlayTogether installer for Windows. One time; afterwards use the "PlayTogether" and
# "Stop PlayTogether" shortcuts (Start menu and desktop).
#
#   From a clone:   double-click install.cmd
#   Without one:    irm https://raw.githubusercontent.com/ashar-viraj/PlayTogether/main/install.ps1 | iex
#
# What it does:
#   1. Downloads the prebuilt PlayTogether.exe from the latest GitHub release, or, if there is
#      none, builds it from source (installing Rust and a C/C++ linker if needed).
#   2. Installs the VB-Audio Virtual Cable (from vb-audio.com): Windows' stand-in for the Linux
#      version's virtual sink, needed to keep this PC's own speakers in sync with the room.
#   3. Allows PlayTogether through Windows Firewall so phones can connect.
#   4. Creates Start / Stop shortcuts.
# On Linux, run: sh install.cmd

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'ashar-viraj/PlayTogether'
$AppDir = Join-Path $env:LOCALAPPDATA 'PlayTogether'
$Exe = Join-Path $AppDir 'Playtogether.exe'

function Say($m)  { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

if ($env:OS -ne 'Windows_NT') { Die 'this is not Windows: run: sh install.cmd' }
if (-not [Environment]::Is64BitOperatingSystem) { Die 'PlayTogether needs 64-bit Windows.' }

New-Item -ItemType Directory -Force -Path $AppDir | Out-Null

# A running copy locks the exe; stop it (its audio output is restored at the end).
$wasRunning = $false
if (Get-Process Playtogether -ErrorAction SilentlyContinue) {
    Say 'Stopping the running PlayTogether...'
    Get-Process Playtogether | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    $wasRunning = $true
}

# ------------------------------------------------------------------ 1a. prebuilt binary
$installed = $false
$download = Join-Path $env:TEMP 'playtogether-download.exe'
try {
    Say 'Downloading the latest PlayTogether release...'
    Invoke-WebRequest "https://github.com/$Repo/releases/latest/download/playtogether-windows-x64.exe" -OutFile $download -UseBasicParsing
    & $download --version *> $null
    if ($LASTEXITCODE -eq 0) {
        Move-Item -Force $download $Exe
        $installed = $true
        Say 'Installed the prebuilt binary.'
    } else {
        Warn 'The downloaded binary did not run; building from source instead.'
    }
} catch {
    Say 'No prebuilt release available; building from source instead.'
}
Remove-Item $download -ErrorAction SilentlyContinue

# ------------------------------------------------------------------ 1b. build from source
function Test-Msvc {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $false }
    $paths = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $linker = $paths | Where-Object { $_ -and (Test-Path "$_\VC\Tools\MSVC\*\bin\Hostx64\x64\link.exe") }
    $sdk = Test-Path "${env:ProgramFiles(x86)}\Windows Kits\10\Lib\*\um\x64\kernel32.lib"
    return [bool]($linker -and $sdk)
}

function Find-MinGW {
    foreach ($dir in @('C:\msys64\ucrt64\bin', 'C:\msys64\mingw64\bin')) {
        if ((Test-Path "$dir\gcc.exe") -and (Test-Path "$dir\dlltool.exe")) { return $dir }
    }
    $gcc = Get-Command gcc.exe -ErrorAction SilentlyContinue
    if ($gcc -and (Get-Command dlltool.exe -ErrorAction SilentlyContinue)) { return (Split-Path $gcc.Source) }
    return $null
}

if (-not $installed) {
    # Source: this clone, or the latest source from GitHub.
    if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'Cargo.toml'))) {
        $src = $PSScriptRoot
    } else {
        Say 'Fetching the source...'
        $work = Join-Path $env:TEMP 'playtogether-src'
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $work | Out-Null
        Invoke-WebRequest "https://github.com/$Repo/archive/refs/heads/main.zip" -OutFile "$work\src.zip" -UseBasicParsing
        Expand-Archive "$work\src.zip" -DestinationPath $work -Force
        $src = (Get-ChildItem $work -Directory | Select-Object -First 1).FullName
    }

    # Rust.
    $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
        Say 'Installing Rust (rustup, for your user only)...'
        $init = Join-Path $env:TEMP 'rustup-init.exe'
        Invoke-WebRequest 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $init -UseBasicParsing
        & $init -y --profile minimal --default-toolchain none | Out-Host
        Remove-Item $init -ErrorAction SilentlyContinue
    }

    # A linker: Microsoft's (MSVC) if it's installed and complete, else MinGW (e.g. MSYS2),
    # else install Microsoft's C++ Build Tools.
    $toolchain = $null
    if (Test-Msvc) {
        $toolchain = 'stable-x86_64-pc-windows-msvc'
    } elseif ($mingw = Find-MinGW) {
        $toolchain = 'stable-x86_64-pc-windows-gnu'
        $env:Path = "$mingw;$env:Path"
    } else {
        Say "Installing Microsoft C++ Build Tools (needed to build; large download, asks for admin)..."
        winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-package-agreements --accept-source-agreements `
            --override '--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended' | Out-Host
        if (-not (Test-Msvc)) { Die 'the C++ Build Tools did not install. Install "Desktop development with C++" from https://visualstudio.microsoft.com/visual-cpp-build-tools/ and re-run.' }
        $toolchain = 'stable-x86_64-pc-windows-msvc'
    }
    Say "Using Rust toolchain $toolchain"
    & rustup toolchain install $toolchain --profile minimal | Out-Host

    Say 'Building PlayTogether (a few minutes the first time)...'
    & cargo "+$toolchain" build --release --manifest-path "$src\Cargo.toml" --target-dir "$src\target" | Out-Host
    if ($LASTEXITCODE -ne 0) { Die 'the build failed (see above).' }
    Copy-Item -Force "$src\target\release\Playtogether.exe" $Exe
    Say 'Built and installed.'
}

# ------------------------------------------------------------------ 2+3. VB-Cable and firewall (one admin prompt)
$hasCable = [bool](Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue |
    Where-Object FriendlyName -match 'VB-Audio Virtual Cable')
$hasRule = [bool](Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue | Where-Object Program -eq $Exe)

$elevated = @()
if (-not $hasCable) {
    Say 'Downloading the VB-Audio Virtual Cable (vb-audio.com)...'
    $cableDir = Join-Path $env:TEMP 'playtogether-vbcable'
    Remove-Item -Recurse -Force $cableDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $cableDir | Out-Null
    $setup = $null
    foreach ($v in 46, 45, 44, 43) {
        try {
            Invoke-WebRequest "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack$v.zip" -OutFile "$cableDir\cable.zip" -UseBasicParsing
            Expand-Archive "$cableDir\cable.zip" -DestinationPath "$cableDir\pkg" -Force
            $setup = "$cableDir\pkg\VBCABLE_Setup_x64.exe"
            break
        } catch { }
    }
    if ($setup -and (Test-Path $setup)) {
        $sig = Get-AuthenticodeSignature $setup
        if ($sig.Status -eq 'Valid' -and $sig.SignerCertificate.Subject -match 'BUREL') {
            $elevated += "Start-Process -FilePath '$setup' -ArgumentList '-i','-h' -Wait"
        } else {
            Warn 'The VB-Cable download is not signed by VB-Audio; skipping it.'
        }
    } else {
        Warn "Couldn't download VB-Cable. Get it from https://vb-audio.com/Cable/ later."
    }
}
if (-not $hasRule) {
    $elevated += "New-NetFirewallRule -DisplayName 'PlayTogether' -Direction Inbound -Program '$Exe' -Action Allow -Profile Private,Domain | Out-Null"
}
if ($elevated.Count -gt 0) {
    Say 'Windows will ask for admin permission to install the audio cable driver / allow phones to connect...'
    $script = Join-Path $env:TEMP 'playtogether-admin.ps1'
    Set-Content -Path $script -Value ($elevated -join "`r`n") -Encoding UTF8
    try {
        Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$script`""
    } catch {
        Warn 'Admin permission was declined.'
    }
    Remove-Item $script -ErrorAction SilentlyContinue
}
if (-not $hasCable) {
    $hasCable = [bool](Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue |
        Where-Object FriendlyName -match 'VB-Audio Virtual Cable')
    if ($hasCable) {
        Say 'VB-Cable installed.'
    } else {
        Warn "VB-Cable isn't installed: PlayTogether still works, but this PC's own speakers will play"
        Warn 'ahead of the phones. Re-run this installer (or install it from https://vb-audio.com/Cable/).'
    }
}

# ------------------------------------------------------------------ 4. launchers and shortcuts
$startCmd = Join-Path $AppDir 'Start PlayTogether.cmd'
$stopCmd = Join-Path $AppDir 'Stop PlayTogether.cmd'
Set-Content -Path $startCmd -Encoding ASCII -Value @'
@echo off
rem Starts PlayTogether in its own window and opens the dashboard.
tasklist /FI "IMAGENAME eq Playtogether.exe" 2>nul | find /I "Playtogether.exe" >nul
if not errorlevel 1 (
    start "" http://localhost:8090/host
    exit /b
)
start "PlayTogether" "%~dp0Playtogether.exe"
ping -n 3 127.0.0.1 >nul
start "" http://localhost:8090/host
'@
Set-Content -Path $stopCmd -Encoding ASCII -Value @'
@echo off
rem Stops PlayTogether and switches Windows back to your normal speakers.
taskkill /IM Playtogether.exe /F >nul 2>&1
if errorlevel 1 (echo PlayTogether wasn't running.) else (echo PlayTogether stopped.)
ping -n 2 127.0.0.1 >nul
"%~dp0Playtogether.exe" --restore-output
ping -n 3 127.0.0.1 >nul
'@

$shell = New-Object -ComObject WScript.Shell
$menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'PlayTogether'
New-Item -ItemType Directory -Force -Path $menu | Out-Null
$desktop = [Environment]::GetFolderPath('Desktop')
foreach ($place in $menu, $desktop) {
    $s = $shell.CreateShortcut((Join-Path $place 'PlayTogether.lnk'))
    $s.TargetPath = $startCmd
    $s.WorkingDirectory = $AppDir
    $s.IconLocation = "$env:SystemRoot\System32\SndVol.exe,0"
    $s.Description = "Play this PC's audio on every phone and PC in the room, in sync"
    $s.Save()
    $s = $shell.CreateShortcut((Join-Path $place 'Stop PlayTogether.lnk'))
    $s.TargetPath = $stopCmd
    $s.WorkingDirectory = $AppDir
    $s.IconLocation = "$env:SystemRoot\System32\shell32.dll,27"
    $s.Description = 'Stop PlayTogether and restore your normal speakers'
    $s.Save()
}

if ($wasRunning) { & $Exe --restore-output | Out-Null }

Say "Done! Installed to $AppDir"
Say 'Start it with the "PlayTogether" shortcut (desktop or Start menu); stop it with "Stop PlayTogether".'
