<#
.SYNOPSIS
  Install the TunnelRez kit for the current user: copy this folder to %LOCALAPPDATA%\Programs\TunnelRez and
  add a Start menu shortcut "TunnelRez".

.DESCRIPTION
  Run it from the unpacked kit. No administrator rights are needed. The drivers are not installed here: the app's
  "Install TV speaker" and TV monitor "Install" buttons do that (one UAC prompt each), from the drivers folder copied
  next to the exe. TunnelRez must not be running while this copies over an older install.

.PARAMETER To
  The install folder (default %LOCALAPPDATA%\Programs\TunnelRez).
#>
[CmdletBinding()]
param([string] $To = (Join-Path $env:LOCALAPPDATA 'Programs\TunnelRez'))
$ErrorActionPreference = 'Stop'
$from = $PSScriptRoot
foreach ($need in 'tunnelrez.exe', 'tunnelrez-send.exe', 'drivers\steam-streaming-speakers\x64\SteamStreamingSpeakers.inf', 'drivers\parsec-vdd\parsec-vdd-0.45.0.0.exe') {
    if (-not (Test-Path (Join-Path $from $need))) { throw "not a TunnelRez kit: $need is missing in $from" }
}
New-Item -ItemType Directory -Force -Path $To | Out-Null
Copy-Item -Path (Join-Path $from '*') -Destination $To -Recurse -Force
$menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'TunnelRez'
New-Item -ItemType Directory -Force -Path $menu | Out-Null
$sh = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $menu 'TunnelRez.lnk'))
$sh.TargetPath = Join-Path $To 'tunnelrez.exe'
$sh.WorkingDirectory = $To
$sh.Save()
Write-Host "installed to $To; Start menu: TunnelRez"
