<#
.SYNOPSIS
  Build the TunnelRez Windows installer (Tauri NSIS bundle, per user, no admin) and list what is in it.

.DESCRIPTION
  1. Reads the version from app\Cargo.toml (the one place it is set) and checks that sender\Cargo.toml agrees.
  2. Builds the sender (release) unless -NoBuild, and copies it to app\binaries\tunnelrez-send-<triple>.exe, the
     name Tauri's externalBin wants. The installer puts it beside tunnelrez.exe as tunnelrez-send.exe; the app finds
     it there by itself (settings.json keeps "sender_path": null).
  3. Checks the bundled drivers against their SHA256SUMS and the Parsec installer's signature (it runs elevated).
  4. Runs `cargo tauri build` with app\installer\tauri.installer.json (bundle settings and the NSIS hooks in
     app\installer\hooks.nsh) and copies the setup exe to dist\.
  5. Lists the installer's contents: with 7-Zip (`-SevenZip`, or 7z.exe on PATH / in Program Files) when there is
     one, else the files the generated NSIS script installs.

  It never runs the installer. Unsigned (no code-signing certificate).

.PARAMETER Out
  Where the setup exe goes (default dist\ under the repo).
.PARAMETER NoBuild
  Use the sender release build already there (the app is always built by `cargo tauri build`).
.PARAMETER SevenZip
  Path to 7z.exe, for the content listing.

.EXAMPLE
  .\tools\build-installer.ps1
#>
[CmdletBinding()]
param(
    [string] $Out = '',
    [switch] $NoBuild,
    [string] $SevenZip = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Out) { $Out = Join-Path $repo 'dist' }
$triple = 'x86_64-pc-windows-msvc'

function Get-CrateVersion([string] $toml) {
    $m = Select-String -Path $toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $m) { throw "no version in $toml" }
    $m.Matches[0].Groups[1].Value
}

# PINNED: see docs/PINNED.md (tauri-cli 2.12.4, whose bundler 2.10.4 NSIS template app\installer\hooks.nsh is written for)
$tauriCli = '2.12.4'
$have = (cargo tauri --version 2>$null) -replace '^tauri-cli\s+', ''
if ($have -ne $tauriCli) {
    throw "tauri-cli $tauriCli is needed (found '$have'): cargo install tauri-cli --version $tauriCli --locked"
}

# 1. Version: one place.
$version = Get-CrateVersion (Join-Path $repo 'app\Cargo.toml')
$senderVersion = Get-CrateVersion (Join-Path $repo 'sender\Cargo.toml')
if ($senderVersion -ne $version) {
    throw "sender\Cargo.toml says $senderVersion, app\Cargo.toml says ${version}; set the sender to $version"
}
Write-Host "TunnelRez $version"

# 2. The sender, where externalBin looks for it.
if (-not $NoBuild) {
    Push-Location (Join-Path $repo 'sender')
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed in sender' }
    } finally { Pop-Location }
}
$sender = Join-Path $repo 'sender\target\release\tunnelrez-send.exe'
if (-not (Test-Path $sender)) { throw "missing $sender (build the sender first)" }
$bin = Join-Path $repo 'app\binaries'
New-Item -ItemType Directory -Force -Path $bin | Out-Null
Copy-Item -LiteralPath $sender -Destination (Join-Path $bin "tunnelrez-send-$triple.exe") -Force

# 3. The drivers go in unchanged.
foreach ($name in 'steam-streaming-speakers', 'parsec-vdd') {
    Push-Location (Join-Path $repo "drivers\$name")
    try {
        foreach ($line in Get-Content 'SHA256SUMS') {
            if (-not $line.Trim()) { continue }
            $want, $file = $line -split '\s+', 2
            $got = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
            if ($got -ne $want) { throw "drivers\$name\$file does not match SHA256SUMS" }
        }
    } finally { Pop-Location }
}
$parsec = Join-Path $repo 'drivers\parsec-vdd\parsec-vdd-0.45.0.0.exe'
$sig = Get-AuthenticodeSignature $parsec
if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'CN="Parsec Cloud, Inc\."') {
    throw "the Parsec installer's signature is not valid: $($sig.Status), $($sig.SignerCertificate.Subject)"
}
Write-Host 'drivers: SHA-256 and the Parsec signature match'

# 4. The bundle.
Push-Location (Join-Path $repo 'app')
try {
    cargo tauri build --bundles nsis --config installer/tauri.installer.json -- --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo tauri build failed' }
} finally { Pop-Location }
$nsisDir = Join-Path $repo 'app\target\release\bundle\nsis'
$setup = Get-ChildItem -LiteralPath $nsisDir -Filter "TunnelRez_${version}_x64-setup.exe" | Select-Object -First 1
if (-not $setup) { throw "no TunnelRez_${version}_x64-setup.exe in $nsisDir" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$dest = Join-Path $Out $setup.Name
Copy-Item -LiteralPath $setup.FullName -Destination $dest -Force
Write-Host "installer: $dest ($((Get-Item $dest).Length) bytes, sha256 $((Get-FileHash $dest -Algorithm SHA256).Hash.ToLower()))"

# 5. What is in it.
if (-not $SevenZip) {
    $SevenZip = @((Get-Command 7z.exe -ErrorAction SilentlyContinue | ForEach-Object Source),
        "$env:ProgramFiles\7-Zip\7z.exe") | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}
if ($SevenZip) {
    Write-Host "contents ($SevenZip l):"
    & $SevenZip l $dest
} else {
    $nsi = Join-Path $repo 'app\target\release\nsis\x64\installer.nsi'
    Write-Host "contents (no 7-Zip: the files $nsi installs):"
    Select-String -Path $nsi -Pattern '^\s*File\s+(/a\s+)?"/oname=([^"]+)"|^\s*File\s+"([^"]+)"' | ForEach-Object {
        '  ' + $_.Line.Trim()
    }
}
