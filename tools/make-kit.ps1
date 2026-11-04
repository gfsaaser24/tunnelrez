<#
.SYNOPSIS
  Build the TunnelRez kit: the tray app, the sender, the TV speaker driver and the TV monitor driver's installer in
  one folder (and a zip).

.DESCRIPTION
  Output (default dist\TunnelRez, plus dist\TunnelRez-kit.zip):

    tunnelrez.exe                       the tray app (finds the sender beside it)
    tunnelrez-send.exe                  the sender (finds the driver folder beside it)
    install.ps1                          copies the kit to %LOCALAPPDATA%\Programs\TunnelRez and adds a
                                         Start menu shortcut (run it yourself; this script never does)
    drivers\steam-streaming-speakers\    the TV speaker driver, unchanged (README.md, SHA256SUMS, x64\*)
    drivers\parsec-vdd\                  the TV monitor driver's installer, unchanged (README.md, SHA256SUMS, *.exe)

  The bundled driver files are checked against SHA256SUMS after the copy, and the Parsec installer's signature must
  be valid and from Parsec. At the end the kit's own sender runs `audio-sink status` and `vdd status` (read only) to
  show that it finds both beside itself.

  FFmpeg is not in the kit: the sender uses ffmpeg.exe from PATH, as before.

.PARAMETER Out
  The kit folder (default dist\TunnelRez under the repo).
.PARAMETER NoBuild
  Use the release builds already there.
#>
[CmdletBinding()]
param(
    [string] $Out = '',
    [switch] $NoBuild
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Out) { $Out = Join-Path $repo 'dist\TunnelRez' }

if (-not $NoBuild) {
    foreach ($crate in 'sender', 'app') {
        Write-Host "cargo build --release ($crate)"
        Push-Location (Join-Path $repo $crate)
        try {
            cargo build --release
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed in $crate" }
        } finally { Pop-Location }
    }
}

$files = @{
    'tunnelrez.exe'      = Join-Path $repo 'app\target\release\tunnelrez.exe'
    'tunnelrez-send.exe' = Join-Path $repo 'sender\target\release\tunnelrez-send.exe'
    'install.ps1'         = Join-Path $repo 'tools\kit\install.ps1'
}
foreach ($f in $files.Values) { if (-not (Test-Path $f)) { throw "missing $f (build first)" } }

if (Test-Path $Out) { Remove-Item -LiteralPath $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
foreach ($k in $files.Keys) { Copy-Item -LiteralPath $files[$k] -Destination (Join-Path $Out $k) }
foreach ($name in 'steam-streaming-speakers', 'parsec-vdd') {
    $drv = Join-Path $Out "drivers\$name"
    New-Item -ItemType Directory -Force -Path $drv | Out-Null
    Copy-Item -Path (Join-Path $repo "drivers\$name\*") -Destination $drv -Recurse

    # The driver files must be the unchanged originals.
    Push-Location $drv
    try {
        foreach ($line in Get-Content 'SHA256SUMS') {
            if (-not $line.Trim()) { continue }
            $want, $file = $line -split '\s+', 2
            $got = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
            if ($got -ne $want) { throw "$file in the kit does not match SHA256SUMS" }
        }
    } finally { Pop-Location }
}

# The installer runs elevated: it must really be Parsec's.
$sig = Get-AuthenticodeSignature (Join-Path $Out 'drivers\parsec-vdd\parsec-vdd-0.45.0.0.exe')
if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'CN="Parsec Cloud, Inc\."') {
    throw "the Parsec installer's signature is not valid: $($sig.Status), $($sig.SignerCertificate.Subject)"
}

$zip = Join-Path (Split-Path $Out -Parent) 'TunnelRez-kit.zip'
if (Test-Path $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path (Join-Path $Out '*') -DestinationPath $zip

Write-Host "kit: $Out"
Get-ChildItem -LiteralPath $Out -Recurse -File | Sort-Object FullName | ForEach-Object {
    '{0,-58} {1,9}  {2}' -f $_.FullName.Substring($Out.Length + 1), $_.Length, (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower().Substring(0, 16)
}
Write-Host "zip: $zip ($((Get-Item $zip).Length) bytes)"
Write-Host 'the kit sender finds the drivers beside itself (read only):'
& (Join-Path $Out 'tunnelrez-send.exe') audio-sink status
& (Join-Path $Out 'tunnelrez-send.exe') vdd status
