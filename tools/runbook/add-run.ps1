<#
.SYNOPSIS
  Add one real-TV test run to the TunnelRez run book (docs/runbook).

.DESCRIPTION
  Two sources:
  - -TvLog: the TV's per-second STATS (an `adb logcat -d -s LRT` dump taken right after the run) and, if given,
    the GPU samples (-GpuCsv, nvidia-smi csv: core %, encoder %, clock MHz, power W).
  - -FromLog: a JSON log from the app's Log switch (%APPDATA%\TunnelRez\logs\<time>.json, layout
    tunnelrez-log/1). It has the TV's STATS, the GPU, the PC stage times and the Wi-Fi of every second, and the
    settings, so -Id, -Title, -Codec, -Resolution, -Fps, -Encoder and the bitrates are taken from it unless given.
    The app's "Add to run book" button runs this.
  Writes docs/runbook/runs/<id>/run.json, tv-stats.jsonl and gpu.csv, then rebuilds the table in
  docs/runbook/README.md. IP addresses are removed from everything that is saved.
  A log goes in once: run.json keeps the log's file name and SHA-256 (`source`), and the same log again is
  skipped with "already <id> ..." (exit code 0).
  A run is marked (`flags` in run.json, a warning sign in the table) when it is shorter than 60 s, or when what was
  on screen changed during it (the log's `checks`: the test page came or went, or the picture stopped moving).
  -Content defaults to what the log says was on screen the whole time ("motion60 test page" from the app's Test).
  Audio: `config.audio` is the run's audio setting (`on`, `off`, `tv_only`; `null` for a log from before audio).
  `summary.audio` (only when the sender or the TV reported audio) has p50 / p99 / max of the audio end-to-end
  latency, A/V offset (positive = sound behind the picture), buffer, TV delay and output latency, and the last
  values of the underrun / drop / lost totals. Runs without audio keys show dashes in the table.
  Latencies that are estimates are labelled: `summary.pc.e2e_source` / `summary.tv_latency_ms.source` start with
  "est", and the table shows them with a leading approximately-equal sign.

.EXAMPLE
  .\tools\runbook\add-run.ps1 -Id 2026-10-26-03 -Title "H.264 4K60 native, slider 150" -Codec h264 `
    -Resolution 3840x2160 -Fps 60 -Encoder native -RequestedMbps 150 -ConfiguredMbps 60 -Network "Wi-Fi 5 GHz" `
    -Content "motion60 page, full screen" -TvLog h264-tv.log -GpuCsv h264-gpu.csv -Notes "sender capped H.264 at 60"

.EXAMPLE
  .\tools\runbook\add-run.ps1 -RebuildTable   # only rebuild the README table (e.g. after editing a run.json)

.EXAMPLE
  .\tools\runbook\add-run.ps1 -FromLog "$env:APPDATA\TunnelRez\logs\2026-10-26_21-14-03.json" -Content "motion60 page"
#>
[CmdletBinding(DefaultParameterSetName = 'TvLog')]
param(
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [string] $Id,
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [string] $Title,
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [string] $Codec,
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [string] $Resolution,
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [int] $Fps,
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [Parameter(ParameterSetName = 'FromLog')] [string] $Encoder,
    [double] $RequestedMbps,
    [double] $ConfiguredMbps,
    [string] $Network = '',
    [string] $Content = '',
    [string] $SenderBuild = '',
    [string] $ReceiverBuild = '',
    [Parameter(ParameterSetName = 'TvLog', Mandatory)] [string] $TvLog,
    [Parameter(ParameterSetName = 'TvLog')] [string] $GpuCsv = '',
    [Parameter(ParameterSetName = 'FromLog', Mandatory)] [string] $FromLog,
    [string] $Notes = '',
    [int] $SkipSeconds = 2,
    [Parameter(ParameterSetName = 'Rebuild', Mandatory)] [switch] $RebuildTable
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$runsDir = Join-Path $root 'docs\runbook\runs'
$ipPattern = '\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b'
$fromApp = $PSCmdlet.ParameterSetName -eq 'FromLog'
# A run needs this many seconds (the method: 60 s runs).
$minSeconds = 60
$flags = @()
# Native NVENC stamps each frame with the capture API's frame time; the FFmpeg path stamps it when ddagrab hands
# the frame over, after its wait for the next grab tick (up to one frame), so its capture-to-shown time is low.
$e2eNative = 'tv_clock: TV display time minus the frame time from the capture API (native NVENC)'
$e2eFfmpeg = 'est: tv_clock from the FFmpeg grab time; leaves out the wait for the grab tick (up to 1 frame), a lower bound'

function Pct([double[]] $v, [double] $p) {
    $s = @($v | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    return $s[[int][Math]::Min($s.Count - 1, [Math]::Floor($s.Count * $p))]
}
# p50 / p99 / max of the numbers in $v (nulls left out); `$null` when there are none.
function AudioStat([object[]] $v) {
    $d = @($v | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($d.Count -eq 0) { return $null }
    return [ordered]@{ p50 = Pct $d 0.5; p99 = Pct $d 0.99; max = ($d | Measure-Object -Maximum).Maximum }
}
function Median([object[]] $v) {
    $d = @($v | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($d.Count -eq 0) { return $null }
    return Pct $d 0.5
}

# Rebuild the index table in docs/runbook/README.md from every run.json.
function Write-RunTable {
    $all = Get-ChildItem $runsDir -Filter run.json -Recurse | Sort-Object { $_.Directory.Name } |
        ForEach-Object { Get-Content $_.FullName -Raw -Encoding UTF8 | ConvertFrom-Json }
    $dash = [string][char]0x2013
    $approx = [string][char]0x2248
    $warn = [string][char]0x26A0
    $f = { param($x) if ($null -eq $x) { $dash } else { '{0:N1}' -f [double]$x } }
    # A value whose source says "est..." is an estimate: shown with a leading approximately-equal sign.
    $est = { param($v, [bool] $isEst) if ($null -eq $v) { $dash } elseif ($isEst) { "$approx$(& $f $v)" } else { & $f $v } }
    $table = @(
        '| Run | Codec | Res / fps | Encoder | Mb/s asked / sent | fps shown | Decode ms (p50) | TV latency ms | E2E ms (capture to shown) | Gaps > 25 ms per s (median / p95 / max) | Longest gap ms | Late | Dropped | Audio | Audio e2e ms (p50 / p99) | A/V ms (p50) | Audio buf ms (p50) | Audio underruns / drops / lost | Notes |'
        '|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|'
    ) + ($all | ForEach-Object {
            $s = $_.summary; $c = $_.config
            $tvl = & $est $s.tv_latency_ms.median ("$($s.tv_latency_ms.source)" -like 'est*')
            # Runs from before e2e_source: the FFmpeg paths' times are estimates (see $e2eFfmpeg).
            $e2eEst = if ($s.pc.e2e_source) { "$($s.pc.e2e_source)" -like 'est*' } else { "$($c.encoder)" -notmatch 'native' }
            $e2e = & $est $s.pc.e2e_latency_ms $e2eEst
            # Runs from before `flags`: short when the run (with its start-up seconds) is under a minute.
            $runFlags = if ($_.PSObject.Properties['flags']) { @($_.flags) } elseif ($s.seconds + $SkipSeconds -lt $minSeconds) { @("short: $($s.seconds + $SkipSeconds) s") } else { @() }
            $notes = if ($runFlags.Count) { "$warn $($runFlags -join ', '). $($_.notes)" } else { $_.notes }
            # Audio: runs from before audio have neither `config.audio` nor `summary.audio`: dashes.
            $au = $s.audio
            $auMode = if ($c.audio) { if ($au.state -and "$($au.state)" -ne 'on') { "$($c.audio) ($($au.state))" } else { "$($c.audio)" } } else { $dash }
            $auCnt = { param($x) if ($null -eq $x) { $dash } else { '{0:N0}' -f [double]$x } }
            $auLoss = "$(& $auCnt $au.underruns) / $(& $auCnt $au.drops) / $(& $auCnt $au.lost)"
            "| [$($_.id)](runs/$($_.id)/run.json) | $($c.codec) | $($c.resolution) @ $($c.fps) | $($c.encoder) | $(& $f $c.requested_mbps) / $(& $f $s.recv_mbps.median) | $(& $f $s.fps_out.median) | $(& $f $s.decode_ms_p50.median) | $tvl | $e2e | $(& $f $s.arrival_gaps_s.median) / $(& $f $s.arrival_gaps_s.p95) / $(& $f $s.arrival_gaps_s.max) | $(& $f $s.arrival_ms_max.max) | $($s.late_total) | $($s.dropped_total) | $auMode | $(& $f $au.e2e_ms.p50) / $(& $f $au.e2e_ms.p99) | $(& $f $au.av_ms.p50) | $(& $f $au.buf_ms.p50) | $auLoss | $notes |"
        })
    $readme = Join-Path $root 'docs\runbook\README.md'
    $text = Get-Content $readme -Raw -Encoding UTF8
    $begin = '<!-- runs:begin -->'; $end = '<!-- runs:end -->'
    $new = $text.Substring(0, $text.IndexOf($begin) + $begin.Length) + "`n" + ($table -join "`n") + "`n" + $text.Substring($text.IndexOf($end))
    [IO.File]::WriteAllText($readme, $new)
}

if ($PSCmdlet.ParameterSetName -eq 'Rebuild') {
    Write-RunTable
    "run book table rebuilt from $(@(Get-ChildItem $runsDir -Filter run.json -Recurse).Count) runs"
    return
}

$gpuRows = @()
$pcSummary = $null
# The run's audio setting (`on`, `off`, `tv_only`); `$null` for a log from before audio.
$audioMode = $null
if ($fromApp) {
    # ------------------------------------------------------------------ the app's JSON log
    $log = Get-Content $FromLog -Raw -Encoding UTF8 | ConvertFrom-Json
    # slipstream-log/*: a log written before the rename (copied from %APPDATA%\SlipStream); same layout.
    if ("$($log.layout)" -notlike 'tunnelrez-log/*' -and "$($log.layout)" -notlike 'slipstream-log/*') {
        throw "$FromLog is not a TunnelRez log (layout '$($log.layout)')"
    }
    # One log, one run: the same file again (same SHA-256) is not added a second time.
    $source = [ordered]@{ log = Split-Path $FromLog -Leaf; sha256 = (Get-FileHash -LiteralPath $FromLog -Algorithm SHA256).Hash.ToLowerInvariant() }
    $dupe = Get-ChildItem $runsDir -Filter run.json -Recurse -ErrorAction SilentlyContinue |
        ForEach-Object { Get-Content $_.FullName -Raw -Encoding UTF8 | ConvertFrom-Json } |
        Where-Object { $_.source -and $_.source.sha256 -eq $source.sha256 } | Select-Object -First 1
    if ($dupe) {
        "already $($dupe.id) in the run book (the same log, $($source.log))"
        return
    }
    # The TV side of each second is its STATS object, or the text "not connected".
    $connected = @($log.samples | Where-Object { $_.tv -isnot [string] })
    if ($connected.Count -eq 0) { throw "no TV samples in the log (the TV was not connected)" }
    # A log can span setting changes. One run = one configuration: keep the longest stretch of consecutive
    # connected seconds with the same running codec/size/encoder, bitrate and audio setting, so the config and every
    # summary agree. (Logs from before audio have no `settings.audio`: an empty, equal key part.)
    $key = { param($x) $r = $x.settings.running; "$($r.codec_out)|$($r.size)|$($x.settings.encoder)|$($x.settings.bitrate_target_mbps)|$($r.target_mbps)|$($x.settings.audio)" }
    $spans = @(); $cur = @(); $prevT = $null; $prevK = $null
    foreach ($x in $connected) {
        $k = & $key $x
        if ($cur.Count -and ($k -ne $prevK -or $x.t_s -ne $prevT + 1)) { $spans += , $cur; $cur = @() }
        $cur += $x; $prevK = $k; $prevT = $x.t_s
    }
    if ($cur.Count) { $spans += , $cur }
    # Ties: the later stretch (the settings the user ended on).
    $tvSamples = @($spans | Sort-Object @{ e = { $_.Count }; Descending = $true }, @{ e = { $_[-1].t_s }; Descending = $true } | Select-Object -First 1)[0]
    $spanNote = if ($spans.Count -gt 1) { "; seconds $($tvSamples[0].t_s)-$($tvSamples[-1].t_s) of $($log.seconds) (the longest of $($spans.Count) stretches with one configuration)" } else { '' }
    $stats = @($tvSamples | ForEach-Object { ($_.tv | ConvertTo-Json -Compress -Depth 6) -replace $ipPattern, '<ip>' })
    # The TV's own latency (frame received to shown): the app's `tv_latency_ms` (the TV's lat_ms_p50 when its STATS
    # have it, else worked out from the exact latency), or lat_ms_p50 in the STATS of an older log.
    $latRows = @($tvSamples | Select-Object -Skip $SkipSeconds | Where-Object { $null -ne $_.network.tv_latency_ms -or $null -ne $_.tv.lat_ms_p50 })
    $lat = @($latRows | ForEach-Object { if ($null -ne $_.network.tv_latency_ms) { [double]$_.network.tv_latency_ms } else { [double]$_.tv.lat_ms_p50 } })
    $latSource = @($latRows | ForEach-Object { if ($_.network.tv_latency_source) { $_.network.tv_latency_source } else { 'tv lat_ms_p50' } } | Select-Object -Unique) -join ', '
    if (-not $ReceiverBuild) { $ReceiverBuild = if ($log.receiver_build) { "$($log.receiver_build)" } else { 'unknown' } }
    $kept = @($tvSamples | Select-Object -Skip $SkipSeconds)
    $gpuRows = @($kept | Where-Object { $_.pc.gpu -and $null -ne $_.pc.gpu.core_pct } |
        ForEach-Object { $g = $_.pc.gpu; [string]::Format([cultureinfo]::InvariantCulture, '{0}, {1}, {2}, {3}', $g.core_pct, $g.encoder_pct, $g.clock_mhz, $g.power_w) })

    # What ran in the kept stretch.
    $ran = @($tvSamples | Where-Object { $_.settings.running }) | Select-Object -Last 1
    $st = if ($ran) { $ran.settings } else { $log.samples[0].settings }
    $r = $st.running
    if ($st.PSObject.Properties['audio'] -and $st.audio) { $audioMode = "$($st.audio)" }
    if (-not $Codec) { $Codec = if ($r.codec_out) { ($r.codec_out -split '/')[0] } else { $st.codec } }
    if (-not $Resolution -or -not $Fps) {
        $size, $rate = if ($r.size) { $r.size -split '@' } else { $st.resolution, $st.fps }
        if (-not $Resolution) { $Resolution = $size }
        if (-not $Fps) { $Fps = [int]$rate }
    }
    # `path` is "HEVC <middle dot> NVIDIA NVENC native": the encoder is the part after the dot.
    $dot = " $([char]0x00B7) "
    if (-not $Encoder) { $Encoder = if ($r.path -and $r.path.Contains($dot)) { ($r.path -split [regex]::Escape($dot), 2)[1] } else { $st.encoder_label } }
    if (-not $PSBoundParameters.ContainsKey('RequestedMbps') -and $null -ne $st.bitrate_target_mbps) { $RequestedMbps = $st.bitrate_target_mbps }
    if (-not $PSBoundParameters.ContainsKey('ConfiguredMbps') -and $null -ne $r.target_mbps) { $ConfiguredMbps = $r.target_mbps }
    if (-not $Network) {
        $w = $kept[-1].network.pc_wifi
        $Network = if ($w.status -eq 'connected') { "PC Wi-Fi $($w.band) $(($w.phy -split ' ')[0]), $($w.rx_mbps)/$($w.tx_mbps) Mb/s, signal $($w.signal_pct) %" } else { "PC $($w.status)" }
        $tvw = $kept[-1].network.tv_wifi
        if ($tvw -isnot [string]) { $Network += "; TV Wi-Fi $(($tvw.PSObject.Properties | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' ')" }
    }
    if (-not $SenderBuild) { $SenderBuild = "$($log.sender) (app $($log.app_version))" }
    if (-not $Id) {
        # PowerShell 7 reads the ISO time as a DateTime, Windows PowerShell as a string.
        $day = if ($log.started -is [datetime]) { $log.started.ToString('yyyy-MM-dd') } else { "$($log.started)".Substring(0, 10) }
        $taken = @(Get-ChildItem $runsDir -Directory -Filter "$day-*" -ErrorAction SilentlyContinue |
            ForEach-Object { if ($_.Name -match '^\d{4}-\d{2}-\d{2}-(\d+)$') { [int]$Matches[1] } })
        $next = 1 + [int](($taken | Measure-Object -Maximum).Maximum)
        $Id = '{0}-{1:D2}' -f $day, $next
    }
    if (-not $Title) { $Title = "$Codec $Resolution@$Fps $Encoder, $RequestedMbps Mb/s (app log)" }
    if (-not $Notes) { $Notes = "From the app log, $($log.seconds) s$spanNote" } elseif ($spanNote) { $Notes += $spanNote }
    # What was on screen: the log knows it when the app's Test put the test page there the whole time.
    $contents = @($tvSamples | ForEach-Object { if ($_.PSObject.Properties['content']) { "$($_.content)" } else { '' } } | Select-Object -Unique)
    if (-not $Content -and $contents.Count -eq 1 -and $contents[0]) { $Content = $contents[0] }
    if ($tvSamples.Count -lt $minSeconds) { $flags += "short: $($tvSamples.Count) s" }
    $changed = ($contents.Count -gt 1) -or ($log.checks -and $log.checks.content_changed)
    if ($changed) { $flags += 'content changed' }
    $native = "$($r.codec_out)" -match '/native'
    # PC side, medians over the kept seconds.
    $pcSummary = [ordered]@{
        capture_ms         = Median @($kept | ForEach-Object { $_.pc.stages_ms.capture })
        convert_ms         = Median @($kept | ForEach-Object { $_.pc.stages_ms.convert })
        encode_ms          = Median @($kept | ForEach-Object { $_.pc.stages_ms.encode })
        send_ms_p99        = Median @($kept | ForEach-Object { $_.pc.stages_ms.send_p99 })
        capture_to_send_ms = Median @($kept | ForEach-Object { $_.pc.stages_ms.capture_to_send })
        encoder_fps        = Median @($kept | ForEach-Object { $_.pc.encoder_fps })
        sent_mbps          = Median @($kept | ForEach-Object { $_.pc.sent_mbps })
        cpu_pct            = Median @($kept | ForEach-Object { $_.pc.cpu_pct })
        rtt_ms             = Median @($kept | ForEach-Object { $_.network.rtt_ms })
        e2e_latency_ms     = Median @($kept | Where-Object { $_.network.latency_source -eq 'tv_clock' } | ForEach-Object { $_.network.latency_ms })
        e2e_source         = if ($native) { $e2eNative } else { $e2eFfmpeg }
    }
    $method = "App logger (Log switch), one sample per second, $($log.seconds) s; TV-side STATS as the sender received them; the first seconds are start-up and are skipped in the summary."
} else {
    # ------------------------------------------------------------------ logcat dump (+ nvidia-smi csv)
    # Per-second STATS lines -> JSON objects. The first seconds are start-up and are skipped in the summary.
    # slipstream_tv: the TV app from before the rename (dev.slipstream.tv), which the PC still streams to.
    $stats = @(Get-Content $TvLog | Where-Object { $_ -match '(tunnelrez|slipstream)_tv::net: \{' } |
        ForEach-Object { ($_ -replace '^.*?(tunnelrez|slipstream)_tv::net: ', '') -replace $ipPattern, '<ip>' })
    if ($stats.Count -eq 0) { throw "no STATS lines in $TvLog" }
    $lat = @(Get-Content $TvLog | Select-String 'lat_ms_p50=([\d.]+)' | ForEach-Object { [double]$_.Matches[0].Groups[1].Value })
    $latSource = 'tv logcat lat_ms_p50'
    $source = [ordered]@{ log = Split-Path $TvLog -Leaf; sha256 = (Get-FileHash -LiteralPath $TvLog -Algorithm SHA256).Hash.ToLowerInvariant() }
    if ($GpuCsv) { $gpuRows = @(Get-Content $GpuCsv) }
    if ($stats.Count -lt $minSeconds) { $flags += "short: $($stats.Count) s" }
    $method = 'Real TV over the LAN, one sender, 60 s, TV-side STATS once per second; the first seconds are start-up and are skipped in the summary.'
}

$runDir = Join-Path $runsDir $Id
New-Item -ItemType Directory -Force $runDir | Out-Null
[IO.File]::WriteAllLines((Join-Path $runDir 'tv-stats.jsonl'), [string[]]$stats)
$rows = @($stats | ForEach-Object { $_ | ConvertFrom-Json } | Select-Object -Skip $SkipSeconds)
if ($rows.Count -eq 0) { throw "only $($stats.Count) TV seconds; the first $SkipSeconds are start-up" }

function Triple([string] $key) {
    $v = @($rows | ForEach-Object { $_.$key } | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($v.Count -eq 0) { return $null }
    return [ordered]@{ median = Pct $v 0.5; p95 = Pct $v 0.95; max = ($v | Measure-Object -Maximum).Maximum }
}

$last = $rows[-1]
$summary = [ordered]@{
    seconds          = $rows.Count
    decoder          = $last.codec
    fps_out          = Triple 'fps_out'
    recv_mbps        = [ordered]@{ median = [Math]::Round((Pct @($rows | ForEach-Object { [double]$_.recv_kbps }) 0.5) / 1000, 1) }
    decode_ms_p50    = Triple 'decode_ms_p50'
    decode_ms_p99    = Triple 'decode_ms_p99'
    tv_latency_ms    = if ($lat.Count) { [ordered]@{ median = Pct $lat 0.5; max = ($lat | Measure-Object -Maximum).Maximum; source = $latSource } } else { $null }
    arrival_gaps_s   = Triple 'arrival_gaps'
    arrival_ms_max   = Triple 'arrival_ms_max'
    decoder_wait_p99 = Triple 'in_wait_ms_p99'
    dropped_total    = $last.dropped
    late_total       = $last.late
    rxq_dropped      = $last.rxq_dropped
    idr_requests_max = (@($rows | ForEach-Object { $_.idr_sent_s }) | Measure-Object -Maximum).Maximum
}
if ($gpuRows.Count) {
    $g = @($gpuRows | ForEach-Object { $f = $_ -split ',\s*'; [pscustomobject]@{ core = [double]$f[0]; enc = [double]$f[1]; clk = [double]$f[2]; w = [double]$f[3] } })
    [IO.File]::WriteAllLines((Join-Path $runDir 'gpu.csv'), [string[]]$gpuRows)
    $summary.gpu = [ordered]@{
        core_pct_avg    = [Math]::Round(($g.core | Measure-Object -Average).Average)
        encoder_pct_avg = [Math]::Round(($g.enc | Measure-Object -Average).Average)
        clock_mhz       = "$(($g.clk | Measure-Object -Minimum).Minimum)-$(($g.clk | Measure-Object -Maximum).Maximum)"
        power_w         = "$(($g.w | Measure-Object -Minimum).Minimum)-$(($g.w | Measure-Object -Maximum).Maximum)"
    }
}
if ($pcSummary) { $summary.pc = $pcSummary }

# Audio: only when the sender or the TV reported any (`audio` / `a_*` keys); runs without it have no block.
# The app's log carries both sides in each sample's `audio` object; a logcat dump has the TV's `a_*` keys only.
$audioSummary = $null
if ($fromApp) {
    $au = @($kept | Where-Object { $_.PSObject.Properties['audio'] -and $_.audio } | ForEach-Object { $_.audio })
    if ($au.Count) {
        $lastOf = { param($k) $x = @($au | Where-Object { $null -ne $_.$k }); if ($x.Count) { $x[-1].$k } else { $null } }
        $audioSummary = [ordered]@{
            state     = & $lastOf 'state'
            tv_only   = & $lastOf 'tv_only'
            e2e_ms    = AudioStat @($au | ForEach-Object { $_.e2e_ms })
            av_ms     = AudioStat @($au | ForEach-Object { $_.av_ms })
            buf_ms    = AudioStat @($au | ForEach-Object { $_.buf_ms })
            delay_ms  = AudioStat @($au | ForEach-Object { $_.delay_ms })
            out_ms    = AudioStat @($au | ForEach-Object { $_.out_ms })
            underruns = & $lastOf 'underruns'
            drops     = & $lastOf 'drops'
            lost      = & $lastOf 'lost'
        }
    }
} elseif (@($rows | Where-Object { $null -ne $_.a_buf_ms }).Count) {
    $lastTv = { param($k) $x = @($rows | Where-Object { $null -ne $_.$k }); if ($x.Count) { $x[-1].$k } else { $null } }
    $audioSummary = [ordered]@{
        state     = $null
        tv_only   = $null
        e2e_ms    = $null
        av_ms     = $null
        buf_ms    = AudioStat @($rows | ForEach-Object { $_.a_buf_ms })
        delay_ms  = AudioStat @($rows | ForEach-Object { $_.a_delay_ms })
        out_ms    = AudioStat @($rows | ForEach-Object { $_.a_out_ms })
        underruns = & $lastTv 'a_underruns'
        drops     = & $lastTv 'a_drops'
        lost      = & $lastTv 'a_lost'
    }
}
if ($audioSummary) { $summary.audio = $audioSummary }

$run = [ordered]@{
    id      = $Id
    title   = $Title
    config  = [ordered]@{
        codec = $Codec; resolution = $Resolution; fps = $Fps; encoder = $Encoder; audio = $audioMode
        requested_mbps = $RequestedMbps; configured_mbps = $ConfiguredMbps
        network = $Network; content = $Content; sender_build = $SenderBuild; receiver_build = $ReceiverBuild
    }
    method  = $method
    summary = $summary
    notes   = $Notes
    flags   = @($flags)
    source  = $source
}
[IO.File]::WriteAllText((Join-Path $runDir 'run.json'), (($run | ConvertTo-Json -Depth 6) -replace $ipPattern, '<ip>'))

Write-RunTable
$flagWords = if ($flags.Count) { " (marked: $($flags -join ', '))" } else { '' }
"run $Id saved in $runDir$flagWords"
