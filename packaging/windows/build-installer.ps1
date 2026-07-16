#requires -Version 5.1
<#
.SYNOPSIS
    One-shot Windows installer build: compile the release binaries, stage them
    (via stage-release.ps1), and compile halod-setup-x64.exe with Inno Setup.

.DESCRIPTION
    Runs the three stages the CI "windows-installer" job does, end to end:

      1. cargo build --release -p halod -p halod-gui -p halod-broker
      2. packaging\windows\stage-release.ps1   (exes + ffmpeg + DLLs + PawnIO blobs)
      3. ISCC.exe halod.iss                     -> packaging\windows\Output\halod-setup-x64.exe

    Prerequisites (install once):
      * MSYS2 UCRT64 + the build toolchain (see docs/development.md).
      * ffmpeg + the dependency walker used by staging:
          C:\msys64\usr\bin\bash.exe -lc "pacman -S --needed --noconfirm mingw-w64-ucrt-x86_64-ffmpeg mingw-w64-ucrt-x86_64-ntldd"
      * Inno Setup 6:
          winget install --id JRSoftware.InnoSetup
    Pass -InstallDeps to run the ffmpeg/ntldd and Inno Setup installs for you.

.PARAMETER AppVersion
    Version stamped into the installer (ISCC /DAppVersion). Defaults to 0.0.0-dev.

.PARAMETER Ucrt64
    MSYS2 UCRT64 prefix (the directory containing bin\). Source of ffmpeg + its
    DLLs + ntldd for staging, and prepended to PATH so pkg-config-based crates
    resolve their native libs during the build.

.PARAMETER SkipBuild
    Reuse the existing src\target\release binaries instead of rebuilding.

.PARAMETER PluginLicensesFile
    Plugin-repository-generated licenses.txt. Release CI stages this beside the
    downloaded binaries; local builds can pass its path explicitly.

.PARAMETER InstallDeps
    Install the missing prerequisites (ffmpeg + ntldd via pacman, Inno Setup via
    winget) before building.
#>
[CmdletBinding()]
param(
    [string]$AppVersion = "0.0.0-dev",
    [string]$Ucrt64     = "C:\msys64\ucrt64",
    [string]$PluginLicensesFile = "",
    [switch]$SkipBuild,
    [switch]$InstallDeps
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$srcDir   = Join-Path $repoRoot "src"
if (-not $PluginLicensesFile) {
    $siblingNotice = Join-Path (Split-Path $repoRoot -Parent) "HaloDaemon-plugins\licenses.txt"
    if (Test-Path $siblingNotice) { $PluginLicensesFile = $siblingNotice }
}

function Fail($msg) { Write-Error $msg; exit 1 }
function Stage($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }

# Locate ISCC.exe — Inno Setup installs per-machine (Program Files) OR per-user
# (%LOCALAPPDATA%\Programs) depending on how winget elevated; check both plus PATH.
function Find-Iscc {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe")
    )
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

# --- 0. Optional: install prerequisites ---------------------------------------
if ($InstallDeps) {
    Stage "Installing prerequisites"
    $bash = "C:\msys64\usr\bin\bash.exe"
    if (-not (Test-Path $bash)) { Fail "MSYS2 not found at $bash (install MSYS2 first)" }
    & $bash -lc "pacman -S --needed --noconfirm mingw-w64-ucrt-x86_64-ffmpeg mingw-w64-ucrt-x86_64-ntldd"
    if ($LASTEXITCODE -ne 0) { Fail "pacman failed installing ffmpeg/ntldd" }
    # winget returns 0x8A15002B (already installed) as a non-zero exit; tolerate it.
    winget install --id JRSoftware.InnoSetup --accept-source-agreements --accept-package-agreements -e
}

# --- 1. Build release binaries ------------------------------------------------
if ($SkipBuild) {
    Stage "Skipping build (-SkipBuild)"
} else {
    Stage "Building release binaries (halod, halod-gui, halod-broker)"
    if (-not (Test-Path (Join-Path $Ucrt64 "bin"))) {
        Fail "UCRT64 prefix not found: $Ucrt64 (install MSYS2 UCRT64)"
    }
    # Make the UCRT64 toolchain visible so pkg-config / native libs resolve, the
    # same way an MSYS2 UCRT64 shell would. Harmless if the build needs none of it.
    $env:PATH = "$(Join-Path $Ucrt64 'bin');$env:PATH"
    Push-Location $srcDir
    try {
        cargo build --release -p halod -p halod-gui -p halod-broker
        if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
    } finally { Pop-Location }
}

# --- 2. Stage files -----------------------------------------------------------
Stage "Staging installer payload"
$stageArgs = @{ Ucrt64 = $Ucrt64 }
if ($PluginLicensesFile) { $stageArgs.PluginLicensesFile = $PluginLicensesFile }
& (Join-Path $PSScriptRoot "stage-release.ps1") @stageArgs
if ($LASTEXITCODE -ne 0) { Fail "staging failed" }

# --- 3. Compile the installer -------------------------------------------------
Stage "Compiling installer (Inno Setup)"
$iscc = Find-Iscc
if (-not $iscc) {
    Fail "ISCC.exe not found. Install Inno Setup 6 (winget install --id JRSoftware.InnoSetup) or re-run with -InstallDeps."
}
Write-Host "  using $iscc"
& $iscc "/DAppVersion=$AppVersion" (Join-Path $PSScriptRoot "halod.iss")
if ($LASTEXITCODE -ne 0) { Fail "ISCC failed" }

$out = Join-Path $PSScriptRoot "Output\halod-setup-x64.exe"
if (-not (Test-Path $out)) { Fail "installer not produced at $out" }
$size = "{0:N1} MB" -f ((Get-Item $out).Length / 1MB)
Write-Host "`nInstaller ready: $out ($size)" -ForegroundColor Green
