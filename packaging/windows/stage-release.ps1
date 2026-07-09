#requires -Version 5.1
<#
.SYNOPSIS
    Stage HaloDaemon's binaries, the bundled ffmpeg, and PawnIO blobs into
    packaging\windows\staging\.

.DESCRIPTION
    Copies the release executables, the bundled ffmpeg.exe (+ its DLL deps) and
    the PawnIO kernel blobs into packaging\windows\staging\, which is packaged verbatim
    by halod.iss.  halod-gui uses wgpu (D3D12/Vulkan) built into Windows, so the
    two Rust exes need no runtime DLLs of their own — but the LCD video mode
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
    [string]$StagingDir = (Join-Path $PSScriptRoot "staging")
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
$exes = "halod.exe", "halod-gui.exe"
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
foreach ($name in $dllNames) {
    $src = Join-Path $ucrtBin $name
    if (Test-Path $src) {
        Copy-Item $src -Destination $StagingDir -Force
        $copied++
    }
}
if ($copied -eq 0) { Fail "no ffmpeg runtime DLLs collected — ntldd walk produced nothing" }
Write-Host "  dlls  $copied runtime DLL(s) from UCRT64 for ffmpeg"

$size = "{0:N1} MB" -f ((Get-ChildItem $StagingDir -Recurse -File |
    Measure-Object -Property Length -Sum).Sum / 1MB)
Write-Host "Staging complete: $StagingDir ($size)"
