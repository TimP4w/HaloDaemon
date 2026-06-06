#requires -Version 5.1
<#
.SYNOPSIS
    Stage HaloDaemon's binaries and the GTK 4 runtime into installer\staging\.

.DESCRIPTION
    halod-gui links against the MSYS2 UCRT64 GTK 4 / libadwaita stack, which
    is not redistributable from the repo. This script:
      * copies the release executables,
      * walks each exe's DLL dependency tree with ntldd and copies every
        dependency that resolves inside the UCRT64 prefix,
      * copies the GTK runtime resource trees (pixbuf loaders, GTK modules,
        compiled GSettings schemas, the Adwaita / hicolor icon themes),
      * copies the PawnIO SMBus blobs and the UI stylesheet.

    The result, installer\staging\, is packaged verbatim by halod.iss.

    Intended for CI (windows-latest + msys2/setup-msys2), but runnable locally.

.PARAMETER Ucrt64
    MSYS2 UCRT64 prefix — the directory containing bin\, lib\ and share\.

.PARAMETER TargetDir
    Directory holding the release-built executables (cargo target\release).

.PARAMETER StagingDir
    Output directory consumed by halod.iss.
#>
[CmdletBinding()]
param(
    [string]$Ucrt64     = "C:\msys64\ucrt64",
    [string]$TargetDir  = (Join-Path $PSScriptRoot "..\src\target\release"),
    [string]$StagingDir = (Join-Path $PSScriptRoot "staging")
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

function Fail($msg) { Write-Error $msg; exit 1 }

if (-not (Test-Path $Ucrt64))         { Fail "UCRT64 prefix not found: $Ucrt64" }
$ucrtBin = Join-Path $Ucrt64 "bin"
$ntldd   = Join-Path $ucrtBin "ntldd.exe"
if (-not (Test-Path $ntldd))          { Fail "ntldd not found: $ntldd (install mingw-w64-ucrt-x86_64-ntldd)" }

# ntldd resolves dependencies via PATH; make sure the UCRT64 bin is visible.
$env:PATH = "$ucrtBin;$env:PATH"

Write-Host "Staging into $StagingDir"
if (Test-Path $StagingDir) { Remove-Item $StagingDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null

# --- 1. Executables --------------------------------------------------------
$exes = "halod.exe", "halod-gui.exe"
foreach ($exe in $exes) {
    $src = Join-Path $TargetDir $exe
    if (-not (Test-Path $src)) { Fail "missing built executable: $src (run cargo build --release first)" }
    Copy-Item $src -Destination $StagingDir
    Write-Host "  exe   $exe"
}

# --- 2. PawnIO blobs + UI stylesheet ---------------------------------------
# SmbusI801 / SmbusPIIX4 drive chipset SMBus (DRAM/GPU RGB); LpcIO drives
# the SuperIO chip for motherboard fan control + temps.
foreach ($blob in "SmbusI801.bin", "SmbusPIIX4.bin", "LpcIO.bin") {
    Copy-Item (Join-Path $repoRoot "pwnio\$blob") -Destination $StagingDir
    Write-Host "  blob  $blob"
}
# The PawnIO modules are LGPL-2.1-or-later (c) namazso; ship their license text.
Copy-Item (Join-Path $repoRoot "pwnio\COPYING") -Destination (Join-Path $StagingDir "PawnIO-LICENSE.txt")
Write-Host "  asset PawnIO-LICENSE.txt"
Copy-Item (Join-Path $repoRoot "src\ui\style.css") -Destination $StagingDir
Write-Host "  asset style.css"

# --- 3. DLL dependency tree ------------------------------------------------
# ntldd -R prints lines like:  libgtk-4-1.dll => /ucrt64/bin/libgtk-4-1.dll (0x..)
# Take the referenced name; copy it when a same-named file exists in UCRT64\bin
# (this naturally skips system DLLs such as KERNEL32.dll).
$dllNames = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase)
foreach ($exe in $exes) {
    $out = & $ntldd -R (Join-Path $StagingDir $exe) 2>$null
    foreach ($line in $out) {
        if ($line -match '^\s*(\S+\.dll)\s*=>') {
            [void]$dllNames.Add($Matches[1])
        }
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
Write-Host "  dlls  $copied runtime DLL(s) from UCRT64"

# --- 4. GTK runtime resource trees -----------------------------------------
function Copy-Tree($relSource, $relDest) {
    $src = Join-Path $Ucrt64 $relSource
    if (-not (Test-Path $src)) { Write-Host "  skip  $relSource (absent)"; return }
    $dst = Join-Path $StagingDir $relDest
    New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
    Copy-Item $src -Destination $dst -Recurse -Force
    Write-Host "  tree  $relDest"
}
Copy-Tree "lib\gdk-pixbuf-2.0"     "lib\gdk-pixbuf-2.0"
Copy-Tree "lib\gtk-4.0"            "lib\gtk-4.0"
Copy-Tree "share\glib-2.0\schemas" "share\glib-2.0\schemas"
Copy-Tree "share\icons\Adwaita"    "share\icons\Adwaita"
Copy-Tree "share\icons\hicolor"    "share\icons\hicolor"

# Compile the GSettings schemas GTK reads at startup.
$schemas = Join-Path $StagingDir "share\glib-2.0\schemas"
$compiler = Join-Path $ucrtBin "glib-compile-schemas.exe"
if ((Test-Path $schemas) -and (Test-Path $compiler)) {
    & $compiler $schemas
    Write-Host "  schemas compiled"
}

$size = "{0:N1} MB" -f ((Get-ChildItem $StagingDir -Recurse -File |
    Measure-Object -Property Length -Sum).Sum / 1MB)
Write-Host "Staging complete: $StagingDir ($size)"
