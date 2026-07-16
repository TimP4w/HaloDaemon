#requires -Version 5.1
<#
.SYNOPSIS
    Stage HaloDaemon's binaries, the bundled ffmpeg, and PawnIO blobs into
    packaging\windows\staging\.

.DESCRIPTION
    Copies the release executables, the bundled ffmpeg.exe (+ its DLL deps) and
    the PawnIO kernel blobs into packaging\windows\staging\, which is packaged verbatim
    by halod.iss.  halod-gui uses the glow (OpenGL) renderer via opengl32 built
    into Windows, so the Rust exes need no runtime DLLs of their own — but the LCD video mode
    shells out to ffmpeg.exe (placed beside halod.exe), which is a dynamically
    linked MSYS2 build and DOES need its libav* DLLs collected here.

    Intended for CI (windows-latest + msys2/setup-msys2), but runnable locally
    from an MSYS2 UCRT64 install.

.PARAMETER Ucrt64
    MSYS2 UCRT64 prefix — the directory containing bin\. Source of ffmpeg.exe,
    its dependency DLLs, and ntldd (the dependency walker).

.PARAMETER TargetDir
    Directory holding the release-built executables (cargo target\release).

.PARAMETER StagingDir
    Output directory consumed by halod.iss.
#>
[CmdletBinding()]
param(
    [string]$Ucrt64     = "C:\msys64\ucrt64",
    [string]$TargetDir  = (Join-Path $PSScriptRoot "..\..\src\target\release"),
    [string]$StagingDir = (Join-Path $PSScriptRoot "staging"),
    [string]$PluginLicensesDir = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

function Fail($msg) { Write-Error $msg; exit 1 }

if (-not (Test-Path $Ucrt64)) { Fail "UCRT64 prefix not found: $Ucrt64 (install MSYS2 UCRT64)" }
$ucrtBin = Join-Path $Ucrt64 "bin"
$ntldd   = Join-Path $ucrtBin "ntldd.exe"
if (-not (Test-Path $ntldd)) { Fail "ntldd not found: $ntldd (install mingw-w64-ucrt-x86_64-ntldd)" }

# ntldd resolves dependencies via PATH; make sure the UCRT64 bin is visible.
$env:PATH = "$ucrtBin;$env:PATH"

Write-Host "Staging into $StagingDir"
if (Test-Path $StagingDir) { Remove-Item $StagingDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null

# --- 1. Executables -----------------------------------------------------------
# halod-broker.exe is the elevated register-bus broker (see the privilege-
# separation design): the installed on-demand HalodBroker LocalSystem service
# that halod.exe starts via the SCM the first time it needs a register bus,
# while halod.exe itself runs at medium integrity. It must be code-signed
# alongside the other two exes wherever CI signs the staged binaries.
$exes = "halod.exe", "halod-gui.exe", "halod-broker.exe"
foreach ($exe in $exes) {
    $src = Join-Path $TargetDir $exe
    if (-not (Test-Path $src)) { Fail "missing built executable: $src (run cargo build --release first)" }
    Copy-Item $src -Destination $StagingDir
    Write-Host "  exe   $exe"
}

# --- 1b. ffmpeg (bundled for LCD video mode) ----------------------------------
# halod invokes ffmpeg.exe (placed beside it) as a separate subprocess to decode
# local videos for the LCD panel. MSYS2's ffmpeg is a GPL build — compatible with
# HaloDaemon's GPL-3.0 licence; its DLL deps are collected by the ntldd walk below.
$ffmpeg = Join-Path $ucrtBin "ffmpeg.exe"
if (-not (Test-Path $ffmpeg)) { Fail "ffmpeg.exe not in UCRT64 bin: $ffmpeg (install mingw-w64-ucrt-x86_64-ffmpeg)" }
Copy-Item $ffmpeg -Destination $StagingDir
Write-Host "  exe   ffmpeg.exe"
# The licensing story below assumes a GPL (version3) build — verify instead of
# trusting the MSYS2 package to keep its configure flags.
$ffConfig = (& $ffmpeg -version 2>$null | Out-String)
if ($ffConfig -match '--enable-nonfree') {
    Fail "bundled ffmpeg was built with --enable-nonfree — not redistributable"
}
foreach ($flag in "--enable-gpl", "--enable-version3") {
    if ($ffConfig -notmatch [regex]::Escape($flag)) {
        Write-Warning ("ffmpeg build config lacks $flag — the staged COPYING.* texts " +
            "may no longer match this build; see docs/licenses.md section 6")
    }
}
# Ship the upstream licence notice for the bundled binary. MSYS2's package layout
# for this text has moved between versions, so it is vendored in the repo (like the
# PawnIO COPYING below). FFmpeg-LICENSE.md is only a summary that points at the
# COPYING.* texts — ship those texts too, or the GPL/LGPL obligation to convey the
# full licence isn't met.
foreach ($ffFile in "FFmpeg-LICENSE.md", "FFmpeg-README.txt") {
    Copy-Item (Join-Path $PSScriptRoot $ffFile) -Destination (Join-Path $StagingDir $ffFile)
    Write-Host "  asset $ffFile"
}
# The bundled binary is a GPL (version3) build with an LGPL core, so ship the full
# GPLv3 + LGPLv2.1 texts — of the four COPYING.* names FFmpeg-LICENSE.md references,
# the two operative for this build. Reuse the repo's LICENSES/ copies
# (GPLv3 == GPL-3.0-or-later, LGPLv2.1 == LGPL-2.1-or-later).
$ffLicenses = @{
    "COPYING.GPLv3"    = "GPL-3.0-or-later.txt"
    "COPYING.LGPLv2.1" = "LGPL-2.1-or-later.txt"
}
foreach ($dest in $ffLicenses.Keys) {
    Copy-Item (Join-Path $repoRoot "LICENSES\$($ffLicenses[$dest])") `
        -Destination (Join-Path $StagingDir $dest)
    Write-Host "  asset $dest"
}

# --- 2. PawnIO blobs ----------------------------------------------------------
# SmbusI801 / SmbusPIIX4 drive chipset SMBus (DRAM/GPU RGB); LpcIO drives
# the SuperIO chip for motherboard fan control + temps; AMDFamily17 reads the
# AMD Ryzen (Zen) on-die SMN thermal registers.
foreach ($blob in "SmbusI801.bin", "SmbusPIIX4.bin", "LpcIO.bin", "AMDFamily17.bin") {
    Copy-Item (Join-Path $repoRoot "pwnio\$blob") -Destination $StagingDir
    Write-Host "  blob  $blob"
}
# The PawnIO modules are LGPL-2.1-or-later (c) namazso; ship their license text.
Copy-Item (Join-Path $repoRoot "pwnio\COPYING") -Destination (Join-Path $StagingDir "PawnIO-LICENSE.txt")
Write-Host "  asset PawnIO-LICENSE.txt"

# --- 2b. Official plugin notices ---------------------------------------------
if ($PluginLicensesDir -and (Test-Path $PluginLicensesDir)) {
    $dest = Join-Path $StagingDir "ThirdPartyLicenses\Plugins"
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item (Join-Path $PluginLicensesDir "*") -Destination $dest -Recurse -Force
    Write-Host "  licenses official plugins"
}

# --- 3. ffmpeg's runtime DLLs -------------------------------------------------
# ntldd -R prints lines like:  libavcodec-61.dll => /ucrt64/bin/libavcodec-61.dll (0x..)
# Take the referenced name; copy it when a same-named file exists in UCRT64\bin
# (this naturally skips system DLLs such as KERNEL32.dll). Only ffmpeg.exe needs
# this — the Rust exes carry no non-system DLL deps.
$dllNames = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase)
$out = & $ntldd -R (Join-Path $StagingDir "ffmpeg.exe") 2>$null
foreach ($line in $out) {
    if ($line -match '^\s*(\S+\.dll)\s*=>') {
        [void]$dllNames.Add($Matches[1])
    }
}
$copied = 0
$runtimeFiles = [System.Collections.Generic.List[string]]::new()
[void]$runtimeFiles.Add("ffmpeg.exe")
foreach ($name in $dllNames) {
    $src = Join-Path $ucrtBin $name
    if (Test-Path $src) {
        Copy-Item $src -Destination $StagingDir -Force
        $copied++
        [void]$runtimeFiles.Add($name)
    }
}
if ($copied -eq 0) { Fail "no ffmpeg runtime DLLs collected — ntldd walk produced nothing" }
Write-Host "  dlls  $copied runtime DLL(s) from UCRT64 for ffmpeg"

# Record the exact MSYS2 packages that supplied ffmpeg.exe and every staged DLL,
# and carry any license directories installed by those packages.  FFmpeg pulls in
# many separately licensed codec/runtime libraries; treating the entire DLL set as
# if it were only FFmpeg would hide those notices from recipients.
$msysRoot = Split-Path $Ucrt64 -Parent
$pacman = Join-Path $msysRoot "usr\bin\pacman.exe"
if (-not (Test-Path $pacman)) { Fail "pacman not found: $pacman" }
$packages = @{}
foreach ($name in $runtimeFiles) {
    $owner = (& $pacman -Qo "/ucrt64/bin/$name" 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $owner -notmatch ' is owned by (\S+) (\S+)$') {
        Fail "cannot determine the MSYS2 package owning $name"
    }
    $packages[$Matches[1]] = $Matches[2]
}

$thirdPartyDir = Join-Path $StagingDir "ThirdPartyLicenses\MSYS2"
New-Item -ItemType Directory -Force -Path $thirdPartyDir | Out-Null
$manifest = [System.Collections.Generic.List[string]]::new()
$manifest.Add("MSYS2 runtime packages bundled for FFmpeg")
$manifest.Add("==========================================")
$manifest.Add("")
$manifest.Add("Package details and source archives: https://packages.msys2.org/")
$manifest.Add("FFmpeg build and source: https://packages.msys2.org/packages/mingw-w64-ucrt-x86_64-ffmpeg")
$manifest.Add("")
foreach ($package in ($packages.Keys | Sort-Object)) {
    $version = $packages[$package]
    $manifest.Add("$package $version")
    $shortName = $package -replace '^mingw-w64-ucrt-x86_64-', ''
    $licenseDir = Join-Path $Ucrt64 "share\licenses\$shortName"
    if (Test-Path $licenseDir) {
        Copy-Item $licenseDir -Destination (Join-Path $thirdPartyDir $shortName) -Recurse -Force
    } else {
        $manifest.Add("  (the installed MSYS2 package contains no share/licenses/$shortName directory)")
    }
}
$manifest.Add("")
$manifest.Add("FFmpeg configure/build identification:")
$manifest.Add($ffConfig.Trim())
$manifest | Set-Content -Encoding UTF8 (Join-Path $thirdPartyDir "MSYS2-PACKAGES.txt")
Write-Host "  asset ThirdPartyLicenses\MSYS2 ($($packages.Count) package records)"

$size = "{0:N1} MB" -f ((Get-ChildItem $StagingDir -Recurse -File |
    Measure-Object -Property Length -Sum).Sum / 1MB)
Write-Host "Staging complete: $StagingDir ($size)"
