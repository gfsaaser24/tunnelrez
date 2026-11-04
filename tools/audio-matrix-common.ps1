<#
.SYNOPSIS
  Shared code for tools\audio-matrix-local.ps1 (fake_tv on 127.0.0.1) and tools\audio-matrix.ps1 (the real TV).
  Dot-source it; it does nothing on its own.

.DESCRIPTION
  - The combos: the same gate as the app (app\src\gate.rs). The sender's `caps` probe (cached by the sender) says
    which encoder x codec runs at each size on this PC; a combo is allowed when `ok` is true. AMD AMF is never
    allowed without an AMD GPU (the probe says so).
  - The sender command line: the same flags as the app (app\src\stream.rs sender_args): --size WxH --fps 60
    --codec C, --backend native for NVENC native, --encoder nvenc|qsv|mf, x264/x265/svtav1 for CPU, the app's
    default bitrate (1080p 40M, 4K 100M), and the audio flags in $script:AudioArgs (set by the caller).
  - The numbers: the sender's 1 Hz STATS lines (stdout, one JSON object per line). They carry the sender's audio
    keys and the TV's keys (the sender copies the TV's STATS into its line).
#>

Set-StrictMode -Version Latest

# The app's default bitrate per picture (app\src\settings.rs BITRATE_TABLE).
$script:AppMbps = @{ '1920x1080' = 40; '3840x2160' = 100 }
$script:CapsEncoders = @('native', 'nvenc', 'qsv', 'amf', 'mf', 'cpu')
# The sender's audio flags for every combo (the callers set them from their parameters).
$script:AudioArgs = @('--audio', 'on')

function Get-RepoRoot { (Resolve-Path (Join-Path $PSScriptRoot '..')).Path }

function Resolve-SenderDir([string] $Dir) {
    if (-not $Dir) { $Dir = Join-Path (Get-RepoRoot) 'sender\target\release' }
    $exe = Join-Path $Dir 'tunnelrez-send.exe'
    if (-not (Test-Path $exe)) { throw "no $exe (build it: cd sender; cargo build --release)" }
    (Resolve-Path $Dir).Path
}

# The source flags. Default: the Parsec virtual display by its --list-monitors index, so its mode is not changed
# (at 1080p the sender scales the picture). -Extend: --extend, like the app (the sender switches the virtual
# display to the size, or plugs one and unplugs it at exit when none exists).
function Get-SourceArgs([string] $Sender, [switch] $Extend, [int] $Monitor = -1) {
    if ($Extend) { return @('--extend') }
    if ($Monitor -ge 0) { return @('--monitor', "$Monitor") }
    $list = & $Sender --list-monitors 2>&1 | Out-String
    $m = [regex]::Match($list, '(?m)^\s*\[(\d+)\]\s+\S+\s+\d+x\d+.*virtual\(Parsec VDD\)')
    if ($m.Success) { return @('--monitor', $m.Groups[1].Value) }
    return @('--extend')
}

# Every encoder x codec of the caps probe at one size, as objects: Size, Encoder, Codec, Allowed, Reason.
function Get-Combos([string] $Sender, [string[]] $SourceArgs, [string] $Size) {
    $out = & $Sender caps @SourceArgs --size $Size --fps 60 2>$null
    $line = @($out | Where-Object { $_ -match '^\s*\{' }) | Select-Object -Last 1
    if (-not $line) { throw "tunnelrez-send caps --size $Size printed no result" }
    $j = $line | ConvertFrom-Json
    foreach ($r in $j.results) {
        [pscustomobject]@{ Size = $Size; Encoder = $r.encoder; Codec = $r.codec; Allowed = [bool]$r.ok; Reason = $r.reason }
    }
}

# The sender flags for one combo (as the app builds them).
function Get-ComboArgs($Combo) {
    $a = @('--size', $Combo.Size, '--fps', '60', '--codec', $Combo.Codec)
    switch ($Combo.Encoder) {
        'native' { $a += @('--backend', 'native') }
        'cpu' { $a += @('--encoder', @{ h264 = 'x264'; hevc = 'x265'; av1 = 'svtav1' }[$Combo.Codec]) }
        default { $a += @('--encoder', $Combo.Encoder) }
    }
    $a += @('--bitrate', "$($script:AppMbps[$Combo.Size])M") + $script:AudioArgs
    $a
}

function Get-ComboName($Combo) {
    $res = @{ '1920x1080' = '1080p'; '3840x2160' = '4K' }[$Combo.Size]
    $enc = @{ native = 'NVENC native'; nvenc = 'NVENC (FFmpeg)'; qsv = 'QSV (FFmpeg)'; amf = 'AMF (FFmpeg)'; mf = 'Media Foundation (FFmpeg)'; cpu = 'CPU (FFmpeg)' }[$Combo.Encoder]
    $codec = @{ h264 = 'H.264'; hevc = 'HEVC'; av1 = 'AV1' }[$Combo.Codec]
    "$res $codec $enc"
}

# Start a process with no window, stdout and stderr to files.
function Start-Hidden([string] $Exe, [string[]] $ArgList, [string] $Out, [string] $Err) {
    # Start-Process joins the list with spaces: quote what has one (a device name for --audio-sink).
    $quoted = @($ArgList | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } })
    Start-Process -FilePath $Exe -ArgumentList $quoted -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $Out -RedirectStandardError $Err
}

function Stop-Mine($Proc) {
    if ($Proc -and -not $Proc.HasExited) {
        Stop-Process -Id $Proc.Id -Force -ErrorAction SilentlyContinue
        $Proc.WaitForExit(5000) | Out-Null
    }
}

# p50 / p99 / max of numbers (nearest rank). $null when there are none.
function Get-Pct([object[]] $Values, [double] $P) {
    $v = @($Values | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ } | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    $i = [math]::Min($v.Count - 1, [math]::Max(0, [int][math]::Ceiling($P * $v.Count) - 1))
    $v[$i]
}

function Get-Key($o, [string] $k) {
    if ($null -eq $o) { return $null }
    $p = $o.PSObject.Properties[$k]
    if ($p) { $p.Value } else { $null }
}

# The sender's STATS lines (stdout), as objects, the first $Skip seconds left out (start-up).
function Read-Stats([string] $File, [int] $Skip = 3) {
    if (-not (Test-Path $File)) { return @() }
    $rows = foreach ($l in Get-Content $File) {
        if ($l -match '^\s*\{') { try { $l | ConvertFrom-Json } catch { } }
    }
    @($rows | Where-Object { $null -ne $_ } | Select-Object -Skip $Skip)
}

# One table row from a run's STATS and logs.
function Get-Summary($Combo, [object[]] $Stats, [string] $SenderErr, [string] $TvErr, [int] $ExitCode) {
    $col = { param($k) @($Stats | ForEach-Object { Get-Key $_ $k } | Where-Object { $null -ne $_ }) }
    $last = { param($k) $v = @(& $col $k); if ($v.Count) { $v[-1] } else { $null } }
    $p50 = { param($k) Get-Pct (& $col $k) 0.5 }
    $p99 = { param($k) Get-Pct (& $col $k) 0.99 }
    $states = @(& $col 'audio' | Select-Object -Unique)
    $errs = @()
    foreach ($f in @($SenderErr, $TvErr)) {
        if ($f -and (Test-Path $f)) {
            $errs += @(Get-Content $f | Where-Object {
                    $_ -match 'PROTOCOL ERROR|panicked|\[audio\] (stopped|capture error|could not)|\[encoder\] (ended|start failed|demoting|native backend failed)|connection closed|Error:|session error'
                })
        }
    }
    if ($ExitCode -ne 0) { $errs += "sender exit code $ExitCode" }
    [pscustomobject]@{
        Combo        = Get-ComboName $Combo
        CodecOut     = & $last 'codec_out'
        Secs         = $Stats.Count
        VideoFps     = & $p50 'enc_fps'
        AudioState   = ($states -join ',')
        TvOnly       = & $last 'audio_tv_only'
        Route        = & $last 'audio_route'
        Device       = & $last 'audio_dev'
        PcEcho       = & $last 'audio_pc'
        SentPerS     = & $p50 'a_sent_s'
        TvPktsPerS   = & $p50 'a_pkts_s'
        CapGaps      = & $last 'a_cap_gaps'
        Lost         = & $last 'a_lost'
        Underruns    = & $last 'a_underruns'
        Drops        = & $last 'a_drops'
        BufMs        = & $p50 'a_buf_ms'
        NetMsP50     = & $p50 'a_net_ms'
        AudioE2eP50  = & $p50 'audio_e2e_ms'
        AudioE2eP99  = & $p99 'audio_e2e_ms'
        AvMsP50      = & $p50 'av_ms'
        Errors       = $errs
    }
}

# Audio is OK: on, about 200 datagrams/s sent and received, the TV's loss and underrun counters present and 0
# (session totals), no errors.
function Test-AudioOk($r) {
    $r.AudioState -eq 'on' -and $r.Secs -ge 5 -and
    $null -ne $r.SentPerS -and [math]::Abs($r.SentPerS - 200) -le 5 -and
    $null -ne $r.TvPktsPerS -and [math]::Abs($r.TvPktsPerS - 200) -le 5 -and
    $null -ne $r.Lost -and ($r.Lost -as [int]) -eq 0 -and
    $null -ne $r.Underruns -and ($r.Underruns -as [int]) -eq 0 -and
    $r.Errors.Count -eq 0
}

function Format-Num($v, [string] $f = '0.#') { if ($null -eq $v) { '-' } else { ([double]$v).ToString($f, [Globalization.CultureInfo]::InvariantCulture) } }

# ---------------------------------------------------------------- PC speaker mute (read only)

if (-not ('TunnelRezMatrix.Speakers' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace TunnelRezMatrix {
  [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IMMDeviceEnumerator {
    int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr devices);
    int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
  }
  [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IMMDevice {
    int Activate(ref Guid iid, int clsCtx, IntPtr activationParams, [MarshalAs(UnmanagedType.IUnknown)] out object iface);
    int OpenPropertyStore(int access, out IntPtr store);
    int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
  }
  [ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IAudioEndpointVolume {
    int RegisterControlChangeNotify(IntPtr n);
    int UnregisterControlChangeNotify(IntPtr n);
    int GetChannelCount(out uint n);
    int SetMasterVolumeLevel(float db, ref Guid ctx);
    int SetMasterVolumeLevelScalar(float v, ref Guid ctx);
    int GetMasterVolumeLevel(out float db);
    int GetMasterVolumeLevelScalar(out float v);
    int SetChannelVolumeLevel(uint ch, float db, ref Guid ctx);
    int SetChannelVolumeLevelScalar(uint ch, float v, ref Guid ctx);
    int GetChannelVolumeLevel(uint ch, out float db);
    int GetChannelVolumeLevelScalar(uint ch, out float v);
    int SetMute([MarshalAs(UnmanagedType.Bool)] bool mute, ref Guid ctx);
    int GetMute([MarshalAs(UnmanagedType.Bool)] out bool mute);
  }
  [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumerator { }
  public static class Speakers {
    // The default playback endpoint's id and mute state (read only).
    public static string State() {
      var e = (IMMDeviceEnumerator)new MMDeviceEnumerator();
      IMMDevice d;
      Marshal.ThrowExceptionForHR(e.GetDefaultAudioEndpoint(0, 1, out d));
      string id; d.GetId(out id);
      var iid = new Guid("5CDF2C82-841E-4546-9722-0CF74078229A");
      object o;
      Marshal.ThrowExceptionForHR(d.Activate(ref iid, 23, IntPtr.Zero, out o));
      bool mute;
      Marshal.ThrowExceptionForHR(((IAudioEndpointVolume)o).GetMute(out mute));
      return (mute ? "muted" : "not muted") + " (" + id + ")";
    }
  }
}
'@
}

function Get-SpeakerState { [TunnelRezMatrix.Speakers]::State() }

function Get-RestoreFile { Join-Path $env:LOCALAPPDATA 'TunnelRez\audio-restore.json' }
function Get-RouteFile { Join-Path $env:LOCALAPPDATA 'TunnelRez\audio-route.json' }

# A sender that was killed leaves a mute (audio-restore.json) or the virtual sink as the default device
# (audio-route.json). `tunnelrez-send audio-restore` undoes both.
function Repair-Mute([string] $Sender, [string[]] $SourceArgs) {
    if ((Test-Path (Get-RestoreFile)) -or (Test-Path (Get-RouteFile))) {
        & $Sender audio-restore 2>&1 | Out-Null
    }
}

# The markdown table of the rows.
function Format-Table-Md([object[]] $Rows, [switch] $Local) {
    $h = '| combo | backend (codec_out) | s | video fps | audio | route | sent/s | TV pkts/s | cap gaps | lost | underruns | drops | buf ms |'
    if ($Local) { $h += ' cap->arrival ms p50 / p99 |' }
    $h += ' audio e2e ms p50 / p99 | A/V ms p50 | errors |'
    $sep = '|' + ('---|' * ($h.Split('|').Count - 2))
    $lines = @($h, $sep)
    foreach ($r in $Rows) {
        $ok = if (Test-AudioOk $r) { 'OK' } else { 'FAIL' }
        $l = "| $($r.Combo) | $($r.CodecOut) | $($r.Secs) | $(Format-Num $r.VideoFps) | $ok ($($r.AudioState)) | $($r.Route) | $(Format-Num $r.SentPerS) | $(Format-Num $r.TvPktsPerS) | $(Format-Num $r.CapGaps '0') | $(Format-Num $r.Lost '0') | $(Format-Num $r.Underruns '0') | $(Format-Num $r.Drops '0') | $(Format-Num $r.BufMs) |"
        if ($Local) { $l += " $(Format-Num $r.ArrivalP50 '0.0') / $(Format-Num $r.ArrivalP99 '0.0') |" }
        $err = if ($r.Errors.Count) { ($r.Errors | Select-Object -First 2 | ForEach-Object { $_ -replace '\|', '/' -replace '\d{1,3}(\.\d{1,3}){3}', '<ip>' }) -join '; ' } else { '-' }
        $l += " $(Format-Num $r.AudioE2eP50) / $(Format-Num $r.AudioE2eP99) | $(Format-Num $r.AvMsP50) | $err |"
        $lines += $l
    }
    $lines -join "`n"
}
