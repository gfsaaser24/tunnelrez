<#
.SYNOPSIS
  Audio x encoder matrix against the real TV: every encoder x codec x size the app allows on this PC, one at a
  time, with the PC's sound on the TV (`--audio tv-only`).

.DESCRIPTION
  The same matrix as tools\audio-matrix-local.ps1, but the sender streams to the TV app. The combos come from the
  sender's `caps` probe, gated as the app does (app\src\gate.rs); the sender gets the app's flags.

  TV lock (C:\code\.tv-lock): each combo takes the lock before it streams and deletes it after (one combo is one
  short batch). The lock is created atomically and holds this script's name, process id and the time. When
  someone else holds it (younger than 15 min), the script refuses: it stops and prints the combos not run yet
  (run them later with -Only). A lock older than 15 min is stale and is taken over.

  Sound: while each combo streams, the script plays a stream of digital silence on the PC (default playback
  device), so the loopback delivers packets at the real engine timing and nothing can be heard anywhere. -Tone
  plays a quiet 440 Hz tone instead (audible on the TV; on the PC only when the sender captures the PC speakers
  and they are not muted). The sound starts only after the sender logs "[audio] on" (by then a virtual sink is the
  default, or the PC speakers are muted) and stops 2 s before the sender's --duration ends.

  Numbers: the sender's 1 Hz STATS lines. Sender keys: audio, audio_tv_only, a_sent_s, a_cap_gaps, audio_e2e_ms,
  av_ms, enc_fps, codec_out. TV keys (copied into the sender's line): a_pkts_s, a_underruns, a_drops, a_lost,
  a_buf_ms. Writes <OutDir>\matrix.md, matrix.json and the logs of each run. IP addresses are left out of the table.

  Before running: open TunnelRez on the TV (or pass -WakeTv to do it over adb). Nothing else is done on the TV.

.PARAMETER TvHost
  The TV's address (required; no address is stored in the repo).
.PARAMETER Seconds
  Sender --duration per combo (default 30).
.PARAMETER Only
  Run only combos whose name matches this regex (e.g. '^4K HEVC').
.PARAMETER Extend
  Capture with --extend like the app. Default: the Parsec virtual display by index, so its mode is not changed.
.PARAMETER Tone
  Play an audible 440 Hz tone instead of digital silence.
.PARAMETER AudioMode
  The sender's --audio (default tv-only: the PC stays silent).
.PARAMETER AudioSink
  The sender's --audio-sink (default: not passed, so the sender's auto).
.PARAMETER WakeTv
  Before the first combo: adb connect <TvHost>:5555, wake the TV and start the TunnelRez app; Home after the last.

.EXAMPLE
  .\tools\audio-matrix.ps1 -TvHost $env:TV_HOST
.EXAMPLE
  .\tools\audio-matrix.ps1 -TvHost $env:TV_HOST -Only '^1080p' -Seconds 60
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $TvHost,
    [int] $Port = 7878,
    [int] $Seconds = 30,
    [string[]] $Sizes = @('1920x1080', '3840x2160'),
    [string] $Only = '',
    [switch] $Extend,
    [int] $Monitor = -1,
    [switch] $Tone,
    [string] $AudioMode = 'tv-only',
    [string] $AudioSink = '',
    [switch] $WakeTv,
    [string] $LockFile = 'C:\code\.tv-lock',
    [string] $SenderDir = '',
    [string] $OutDir = ''
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'audio-matrix-common.ps1')
$script:AudioArgs = @('--audio', $AudioMode) + $(if ($AudioSink) { @('--audio-sink', $AudioSink) } else { @() })

$dir = Resolve-SenderDir $SenderDir
$sender = Join-Path $dir 'tunnelrez-send.exe'
if (-not $OutDir) { $OutDir = Join-Path $env:TEMP ("tunnelrez-audio-matrix-tv-" + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$lockStale = [TimeSpan]::FromMinutes(15)
$lockText = "audio-matrix.ps1 pid $PID"

# ---------------------------------------------------------------- TV lock

# Take the lock (atomic create). False when someone else holds a fresh one.
function Enter-TvLock {
    for ($try = 0; $try -lt 2; $try++) {
        try {
            $fs = [IO.File]::Open($LockFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            $b = [Text.Encoding]::UTF8.GetBytes("$lockText $((Get-Date).ToString('o'))`n")
            $fs.Write($b, 0, $b.Length)
            $fs.Close()
            return $true
        } catch [IO.IOException] {
            $age = (Get-Date) - (Get-Item $LockFile -ErrorAction SilentlyContinue).LastWriteTime
            $who = (Get-Content $LockFile -Raw -ErrorAction SilentlyContinue)
            if ($age -lt $lockStale) {
                Write-Warning "the TV is locked by: $($who.Trim()) ($([int]$age.TotalMinutes) min ago)"
                return $false
            }
            Write-Warning "stale TV lock ($([int]$age.TotalMinutes) min old: $($who.Trim())); taking it over"
            Remove-Item $LockFile -Force -ErrorAction SilentlyContinue
        }
    }
    $false
}

# Delete the lock, only when it is ours.
function Exit-TvLock {
    $t = Get-Content $LockFile -Raw -ErrorAction SilentlyContinue
    if ($t -and $t.StartsWith("$lockText ")) { Remove-Item $LockFile -Force -ErrorAction SilentlyContinue }
}

# ---------------------------------------------------------------- tone

# 1 s of 440 Hz (an exact number of cycles, so it loops without a click), 48 kHz s16 stereo, amplitude $Amp
# (0 = digital silence).
function New-ToneWav([string] $Path, [double] $Amp) {
    $rate = 48000; $n = $rate
    $ms = New-Object IO.MemoryStream
    $w = New-Object IO.BinaryWriter($ms)
    $w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + $n * 4)); $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
    $w.Write([int]16); $w.Write([int16]1); $w.Write([int16]2); $w.Write([int]$rate); $w.Write([int]($rate * 4)); $w.Write([int16]4); $w.Write([int16]16)
    $w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int]($n * 4))
    for ($i = 0; $i -lt $n; $i++) {
        $s = [int16]([math]::Round($Amp * 32767 * [math]::Sin(2 * [math]::PI * 440 * $i / $rate)))
        $w.Write($s); $w.Write($s)
    }
    $w.Flush()
    [IO.File]::WriteAllBytes($Path, $ms.ToArray())
}

# ---------------------------------------------------------------- run

$adbTarget = "${TvHost}:5555"
if ($WakeTv) {
    adb connect $adbTarget | Out-Null
    adb -s $adbTarget shell input keyevent KEYCODE_WAKEUP | Out-Null
    $act = @(adb -s $adbTarget shell cmd package resolve-activity --brief dev.tunnelrez.tv) | Select-Object -Last 1
    if ($act -notmatch '/') { throw "TunnelRez (dev.tunnelrez.tv) is not installed on the TV" }
    adb -s $adbTarget shell am start -n $act.Trim() | Out-Null
    Start-Sleep -Seconds 3
}

Repair-Mute $sender @('--monitor', '0')
$speakersBefore = Get-SpeakerState
$src = Get-SourceArgs $sender -Extend:$Extend -Monitor $Monitor
Write-Host "source: $($src -join ' '); speakers before: $speakersBefore; logs: $OutDir"
# The caps probe can run real encodes (when not cached): it takes the lock too.
if (-not (Enter-TvLock)) { Write-Warning 'refusing to run: someone else holds the TV lock'; exit 3 }
try { $all = foreach ($s in $Sizes) { Get-Combos $sender $src $s } } finally { Exit-TvLock }
$skipped = @($all | Where-Object { -not $_.Allowed })
$combos = @($all | Where-Object { $_.Allowed -and (Get-ComboName $_) -match $Only })
Write-Host "allowed: $($combos.Count); not allowed: $($skipped.Count); $Seconds s each"
$wav = Join-Path $OutDir 'tone.wav'
New-ToneWav $wav $(if ($Tone) { 0.05 } else { 0.0 })

$rows = @()
$notRun = @()
$i = 0
try {
    foreach ($c in $combos) {
        $i++
        $name = Get-ComboName $c
        if ($notRun.Count -or -not (Enter-TvLock)) { $notRun += $name; continue }
        $tag = ($name -replace '[^A-Za-z0-9]+', '-').Trim('-')
        $sOut = "$OutDir\$tag.sender.out"; $sErr = "$OutDir\$tag.sender.err"
        Write-Host "[$i/$($combos.Count)] $name"
        $snd = $null; $player = $null; $code = -1
        try {
            $argv = @('--tv', $TvHost, '--port', "$Port") + $src + (Get-ComboArgs $c) + @('--duration', "$Seconds")
            "tunnelrez-send $(($argv -join ' ').Replace($TvHost, '<tv>'))" | Set-Content "$OutDir\$tag.cmd.txt"
            $t0 = Get-Date
            $snd = Start-Hidden $sender $argv $sOut $sErr
            $toneEnd = $t0.AddSeconds($Seconds - 2)
            while (-not $snd.HasExited -and (Get-Date) -lt $toneEnd -and -not ((Test-Path $sErr) -and (Select-String -Quiet -Path $sErr -Pattern '\[audio\] on:'))) {
                Start-Sleep -Milliseconds 100
            }
            if (-not $snd.HasExited -and (Get-Date) -lt $toneEnd) {
                $player = New-Object Media.SoundPlayer $wav
                $player.PlayLooping()
            }
            while (-not $snd.HasExited -and (Get-Date) -lt $toneEnd) { Start-Sleep -Milliseconds 200 }
            if ($player) { $player.Stop(); $player.Dispose(); $player = $null }
            if (-not $snd.WaitForExit(90 * 1000)) { Write-Warning "${name}: sender still running; stopping it" }
            Stop-Mine $snd
            $snd.WaitForExit()
            $code = $snd.ExitCode
        } finally {
            if ($player) { $player.Stop(); $player.Dispose() }
            Stop-Mine $snd
            Exit-TvLock
        }
        $r = Get-Summary $c (Read-Stats $sOut) $sErr '' $code
        $rows += $r
        Write-Host ("    video {0} fps, audio {1}, {2}/s sent, {3}/s on the TV, lost {4}, underruns {5}, drops {6}, e2e {7} ms, A/V {8} ms{9}" -f `
            (Format-Num $r.VideoFps), $r.AudioState, (Format-Num $r.SentPerS), (Format-Num $r.TvPktsPerS), (Format-Num $r.Lost '0'),
            (Format-Num $r.Underruns '0'), (Format-Num $r.Drops '0'), (Format-Num $r.AudioE2eP50), (Format-Num $r.AvMsP50),
            $(if ($r.Errors.Count) { "; errors: $($r.Errors.Count)" } else { '' }))
        Start-Sleep -Seconds 2
    }
} finally {
    Repair-Mute $sender $src
    Exit-TvLock
    if ($WakeTv) { adb -s $adbTarget shell input keyevent KEYCODE_HOME | Out-Null }
}

$speakersAfter = Get-SpeakerState
$restoreLeft = (Test-Path (Get-RestoreFile)) -or (Test-Path (Get-RouteFile))
$md = @("# Audio matrix, real TV", '',
    "$(Get-Date -Format 'yyyy-MM-dd HH:mm'); $Seconds s per combo; source ``$($src -join ' ')``; ``$($script:AudioArgs -join ' ')``; audible tone: $([bool]$Tone).", '',
    (Format-Table-Md $rows), '',
    'Not allowed on this PC (the caps probe, as the app gates them):', '')
$md += $skipped | ForEach-Object { "- $(Get-ComboName $_): $($_.Reason)" }
if ($notRun.Count) { $md += @('', 'Not run (the TV was locked by someone else):', '') + ($notRun | ForEach-Object { "- $_" }) }
$md += @('', "Speakers before: $speakersBefore. After: $speakersAfter. Restore file left: $restoreLeft.")
$md -join "`n" | Set-Content "$OutDir\matrix.md" -Encoding utf8
$rows | ConvertTo-Json -Depth 4 | Set-Content "$OutDir\matrix.json" -Encoding utf8
Get-Content "$OutDir\matrix.md"
if ($notRun.Count) { Write-Warning "not run (TV locked): $($notRun -join ', '). Run them later with -Only."; exit 3 }
if ($restoreLeft -or $speakersAfter -ne $speakersBefore) { Write-Warning "speaker mute not restored: $speakersAfter (before: $speakersBefore)"; exit 2 }
if (@($rows | Where-Object { -not (Test-AudioOk $_) }).Count) { exit 1 }
