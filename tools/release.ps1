<#
.SYNOPSIS
  Build the TunnelRez release files locally: the installer, the TV app (optional), SHA256SUMS and release notes.

.DESCRIPTION
  Writes into -Out (default dist\release\<version>):

    TunnelRez_<version>_x64-setup.exe   the Windows installer (tools\build-installer.ps1)
    tunnelrez-tv-<version>.apk          the TV app, with -Apk (receiver\build-and-install.ps1 -NoInstall)
    SHA256SUMS                          `sha256sum -c` format, one line per file above
    RELEASE-NOTES.md                    the CHANGELOG.md section of this version

  The version comes from app\Cargo.toml; CHANGELOG.md must have a "## [<version>]" section. Nothing is published:
  no tag, no GitHub release. To publish, the owner runs (by hand):
    gh release create v<version> --draft --notes-file <Out>\RELEASE-NOTES.md <Out>\*

.PARAMETER Out
  The release folder.
.PARAMETER Apk
  Also build the TV app (needs the Android SDK, NDK, JDK 17 and cargo-ndk; see AGENTS.md).
.PARAMETER NoBuild
  Passed to build-installer.ps1 (use the sender build already there).
#>
[CmdletBinding()]
param(
    [string] $Out = '',
    [switch] $Apk,
    [switch] $NoBuild
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$m = Select-String -Path (Join-Path $repo 'app\Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw 'no version in app\Cargo.toml' }
$version = $m.Matches[0].Groups[1].Value
if (-not $Out) { $Out = Join-Path $repo "dist\release\$version" }

# Release notes: the CHANGELOG section of this version, up to the next "## [".
$changelog = Get-Content (Join-Path $repo 'CHANGELOG.md')
$start = [Array]::FindIndex($changelog, [Predicate[string]] { param($l) $l -match "^## \[$([regex]::Escape($version))\]" })
if ($start -lt 0) { throw "CHANGELOG.md has no '## [$version]' section" }
$end = [Array]::FindIndex($changelog, $start + 1, [Predicate[string]] { param($l) $l -match '^## \[' })
if ($end -lt 0) { $end = $changelog.Length }
$notes = @("# TunnelRez $version", '') + $changelog[($start + 1)..($end - 1)]

if (Test-Path $Out) { Remove-Item -LiteralPath $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null

$build = @{ Out = $Out }
if ($NoBuild) { $build.NoBuild = $true }
& (Join-Path $PSScriptRoot 'build-installer.ps1') @build

if ($Apk) {
    & (Join-Path $repo 'receiver\build-and-install.ps1') -NoInstall
    $built = Join-Path $repo 'receiver\android\app\build\outputs\apk\release\app-release.apk'
    Copy-Item -LiteralPath $built -Destination (Join-Path $Out "tunnelrez-tv-$version.apk")
}

$files = Get-ChildItem -LiteralPath $Out -File | Where-Object { $_.Name -notin 'SHA256SUMS', 'RELEASE-NOTES.md' } |
    Sort-Object Name
$sums = $files | ForEach-Object { '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name }
# LF endings, so `sha256sum -c SHA256SUMS` works on Linux and in Git Bash.
[IO.File]::WriteAllText((Join-Path $Out 'SHA256SUMS'), (($sums -join "`n") + "`n"))
$notes += @('', '## Files', '', '```') + $sums + @('```')
[IO.File]::WriteAllText((Join-Path $Out 'RELEASE-NOTES.md'), (($notes -join "`n") + "`n"))

Write-Host "release files in ${Out}:"
Get-ChildItem -LiteralPath $Out -File | Sort-Object Name | ForEach-Object { '  {0,-40} {1,10}' -f $_.Name, $_.Length }
