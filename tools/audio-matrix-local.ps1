<#
.SYNOPSIS
  Audio x encoder matrix on this PC, against fake_tv on 127.0.0.1 (no TV needed).

.DESCRIPTION
  For every encoder x codec x size the app allows on this PC (the sender's `caps` probe, as in app\src\gate.rs):
  start fake_tv (it runs the TV's own jitter/playout code, and `--tone` plays a quiet tone on the PC from
  AUDIO_START on, so the loopback carries real sound), then the sender with the app's flags and `--audio tv-only`
  (the PC speakers stay muted while it streams; the sender unmutes them after). One combo at a time.

  Records per combo: video fps, audio datagrams/s (sent and received), capture gaps / lost / underruns / drops,
  capture -> arrival ms (p50 / p99, from fake_tv's audio summary), audio end-to-end and A/V ms, and errors.
  Writes <OutDir>\matrix.md (the table), matrix.json, and the logs of each run.

  At the end it checks that %LOCALAPPDATA%\TunnelRez\audio-restore.json is gone and that the default speakers
  are in the same mute state as before.

  Nothing goes to the TV. No window opens. Only processes this script started are stopped.

.PARAMETER Seconds
  Sender --duration per combo (default 15).
.PARAMETER Sizes
  Pictures to test (default 1920x1080 and 3840x2160).
.PARAMETER Only
  Run only combos whose name matches this regex (e.g. '4K.*HEVC').
.PARAMETER Extend
  Capture with --extend like the app. Default: the Parsec virtual display by index, so its mode is not changed.
.PARAMETER Port
  The TCP/UDP port fake_tv listens on (default 17878, not the TV's 7878).

.EXAMPLE
  .\tools\audio-matrix-local.ps1
.EXAMPLE
  .\tools\audio-matrix-local.ps1 -Only '1080p H.264' -Seconds 30
#>
[CmdletBinding()]
param(
    [int] $Seconds = 15,
    [string[]] $Sizes = @('1920x1080', '3840x2160'),
    [string] $Only = '',
    [switch] $Extend,
    [int] $Monitor = -1,
    [int] $Port = 17878,
    [string] $SenderDir = '',
    [string] $OutDir = '',
    [string] $AudioSink = ''
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'audio-matrix-common.ps1')
$script:AudioArgs = @('--audio', 'on') + $(if ($AudioSink) { @('--audio-sink', $AudioSink) } else { @() })

$dir = Resolve-SenderDir $SenderDir
$sender = Join-Path $dir 'tunnelrez-send.exe'
$faketv = Join-Path $dir 'fake_tv.exe'
if (-not (Test-Path $faketv)) { throw "no $faketv (cargo build --release in sender)" }
if (-not $OutDir) { $OutDir = Join-Path $env:TEMP ("tunnelrez-audio-matrix-local-" + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Repair-Mute $sender @('--monitor', '0')
$speakersBefore = Get-SpeakerState
$src = Get-SourceArgs $sender -Extend:$Extend -Monitor $Monitor
Write-Host "source: $($src -join ' '); speakers before: $speakersBefore; logs: $OutDir"

$all = foreach ($s in $Sizes) { Get-Combos $sender $src $s }
$skipped = @($all | Where-Object { -not $_.Allowed })
$combos = @($all | Where-Object { $_.Allowed -and (Get-ComboName $_) -match $Only })
Write-Host "allowed: $($combos.Count); not allowed: $($skipped.Count)"

$rows = @()
$i = 0
try {
    foreach ($c in $combos) {
        $i++
        $name = Get-ComboName $c
        $tag = ($name -replace '[^A-Za-z0-9]+', '-').Trim('-')
        $f = @{ sOut = "$OutDir\$tag.sender.out"; sErr = "$OutDir\$tag.sender.err"; tOut = "$OutDir\$tag.tv.out"; tErr = "$OutDir\$tag.tv.err" }
        Write-Host "[$i/$($combos.Count)] $name"
        $tv = Start-Hidden $faketv @('--listen', "127.0.0.1:$Port", '--once', '--silent-tone') $f.tOut $f.tErr
        $snd = $null
        try {
            for ($w = 0; $w -lt 50 -and -not ((Test-Path $f.tErr) -and (Select-String -Quiet -Path $f.tErr -Pattern 'listening')); $w++) { Start-Sleep -Milliseconds 100 }
            $argv = @('--tv', '127.0.0.1', '--port', "$Port") + $src + (Get-ComboArgs $c) + @('--duration', "$Seconds")
            "tunnelrez-send $($argv -join ' ')" | Set-Content "$OutDir\$tag.cmd.txt"
            $snd = Start-Hidden $sender $argv $f.sOut $f.sErr
            if (-not $snd.WaitForExit(($Seconds + 90) * 1000)) { Write-Warning "${name}: sender still running; stopping it"; Stop-Mine $snd }
            $snd.WaitForExit()
            $code = $snd.ExitCode
            if (-not $tv.WaitForExit(5000)) { Stop-Mine $tv }
        } finally {
            Stop-Mine $snd
            Stop-Mine $tv
        }
        $r = Get-Summary $c (Read-Stats $f.sOut) $f.sErr $f.tErr $code
        $tvLog = if (Test-Path $f.tErr) { Get-Content $f.tErr -Raw } else { '' }
        $m = [regex]::Match($tvLog, 'audio summary: capture->arrival ms p50=([-\d.]+) p99=([-\d.]+)')
        $r | Add-Member ArrivalP50 $(if ($m.Success) { [double]$m.Groups[1].Value } else { $null })
        $r | Add-Member ArrivalP99 $(if ($m.Success) { [double]$m.Groups[2].Value } else { $null })
        $v = [regex]::Match($tvLog, 'summary: hello=.* fps=([\d.]+).* protocol_errors=(\d+)')
        $r | Add-Member TvVideoFps $(if ($v.Success) { [double]$v.Groups[1].Value } else { $null })
        if ($v.Success -and [int]$v.Groups[2].Value -gt 0) { $r.Errors += "fake_tv protocol_errors=$($v.Groups[2].Value)" }
        $rows += $r
        Write-Host ("    video {0} fps, audio {1}, {2}/s sent, {3}/s received, lost {4}, underruns {5}, cap->arrival p50 {6} ms{7}" -f `
            (Format-Num $r.VideoFps), $r.AudioState, (Format-Num $r.SentPerS), (Format-Num $r.TvPktsPerS), (Format-Num $r.Lost '0'),
            (Format-Num $r.Underruns '0'), (Format-Num $r.ArrivalP50 '0.0'), $(if ($r.Errors.Count) { "; errors: $($r.Errors.Count)" } else { '' }))
        Start-Sleep -Seconds 1
    }
} finally {
    Repair-Mute $sender $src
}

$speakersAfter = Get-SpeakerState
$restoreLeft = (Test-Path (Get-RestoreFile)) -or (Test-Path (Get-RouteFile))
$md = @("# Audio matrix, local (sender -> fake_tv on 127.0.0.1)", '',
    "$(Get-Date -Format 'yyyy-MM-dd HH:mm'); $Seconds s per combo; source ``$($src -join ' ')``; ``$($script:AudioArgs -join ' ')``; fake_tv ``--silent-tone``.", '',
    (Format-Table-Md $rows -Local), '',
    'Not allowed on this PC (the caps probe, as the app gates them):', '')
$md += $skipped | ForEach-Object { "- $(Get-ComboName $_): $($_.Reason)" }
$md += @('', "Speakers before: $speakersBefore. After: $speakersAfter. Restore file left: $restoreLeft.")
$md -join "`n" | Set-Content "$OutDir\matrix.md" -Encoding utf8
$rows | ConvertTo-Json -Depth 4 | Set-Content "$OutDir\matrix.json" -Encoding utf8
Get-Content "$OutDir\matrix.md"
if ($restoreLeft -or $speakersAfter -ne $speakersBefore) { Write-Warning "speaker mute not restored: $speakersAfter (before: $speakersBefore)"; exit 2 }
if (@($rows | Where-Object { -not (Test-AudioOk $_) }).Count) { exit 1 }
