# Build the TunnelRez TV receiver (Rust cdylib + APK, package dev.tunnelrez.tv) and install it on the TV.
#   .\build-and-install.ps1                 # build, install, launch on $env:TV_HOST
#   .\build-and-install.ps1 -NoInstall      # build only (APK path printed at the end)
#   .\build-and-install.ps1 -Arm64          # also build arm64-v8a
[CmdletBinding()]
param(
    [string]$Tv = $env:TV_HOST,
    [switch]$NoInstall,
    [switch]$NoLaunch,
    [switch]$Arm64
)
$ErrorActionPreference = 'Stop'
if (-not $NoInstall -and -not $Tv) { throw 'set $env:TV_HOST (e.g. <tv-ip>:5555) or pass -Tv <host:port>, or use -NoInstall' }
if ($Tv -and $Tv -notmatch ':') { $Tv = "${Tv}:5555" }
$root = $PSScriptRoot
$sdk = Join-Path $env:LOCALAPPDATA 'Android\Sdk'

# PINNED: see docs/PINNED.md (JDK 17, NDK 29.0.14206865, API 26)
if (-not $env:JAVA_HOME) {
    # Fall back to the newest JDK 17 from Eclipse Adoptium.
    $adoptium = Join-Path $env:ProgramFiles 'Eclipse Adoptium'
    $jdk = Get-ChildItem $adoptium -Directory -Filter 'jdk-17*' -ErrorAction SilentlyContinue |
        Sort-Object Name | Select-Object -Last 1
    if (-not $jdk) { throw "JAVA_HOME is not set and no JDK 17 was found under $adoptium\. Install JDK 17 or set `$env:JAVA_HOME." }
    $env:JAVA_HOME = $jdk.FullName
}
Write-Host "JDK: $env:JAVA_HOME"
$env:ANDROID_HOME = $sdk
$ndkVersion = '29.0.14206865'
if (-not $env:ANDROID_NDK_HOME) {
    # The pinned NDK; set $env:ANDROID_NDK_HOME to build with another one.
    $ndk = Join-Path $sdk "ndk\$ndkVersion"
    if (-not (Test-Path $ndk)) { throw "No NDK $ndkVersion under $sdk\ndk. Install it: sdkmanager --install `"ndk;$ndkVersion`"" }
    $env:ANDROID_NDK_HOME = $ndk
}
Write-Host "NDK: $env:ANDROID_NDK_HOME"

# local.properties for Gradle
$lp = Join-Path $root 'android\local.properties'
"sdk.dir=$($sdk -replace '\\','/')" | Set-Content -Encoding ascii $lp

# 1) Rust -> android/app/src/main/jniLibs/<abi>/libtunnelrez_tv.so
# Remove libraries from the old crate names (lrt_receiver, slipstream_tv) so they do not end up in the APK.
Get-ChildItem (Join-Path $root 'android\app\src\main\jniLibs') -Recurse -Include 'liblrt_receiver.so', 'libslipstream_tv.so' -ErrorAction SilentlyContinue |
    Remove-Item -Force
$targets = @('-t', 'armeabi-v7a')
if ($Arm64) { $targets += @('-t', 'arm64-v8a') }
Push-Location $root
try {
    & cargo ndk @targets -P 26 -o android/app/src/main/jniLibs build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo ndk failed ($LASTEXITCODE)" }
} finally { Pop-Location }

# 2) APK
Push-Location (Join-Path $root 'android')
try {
    & .\gradlew.bat --quiet assembleRelease
    if ($LASTEXITCODE -ne 0) { throw "gradle failed ($LASTEXITCODE)" }
} finally { Pop-Location }
$apk = Join-Path $root 'android\app\build\outputs\apk\release\app-release.apk'
if (-not (Test-Path $apk)) { throw "APK not found: $apk" }
Write-Host "APK: $apk"
if ($NoInstall) { return }

# 3) Install + launch
& adb connect $Tv | Out-Null
# The TV defaults logcat to errors only (persist.log.tag=E); let the LRT tag through.
& adb -s $Tv shell setprop persist.log.tag.LRT I
& adb -s $Tv install -r $apk
if ($LASTEXITCODE -ne 0) { throw "adb install failed" }
if (-not $NoLaunch) {
    & adb -s $Tv shell input keyevent WAKEUP
    # The old app (dev.slipstream.tv), while still installed, keeps port 7878 in the background: stop it first, or
    # the new app cannot listen and the PC keeps reaching the old one.
    & adb -s $Tv shell am force-stop dev.slipstream.tv
    & adb -s $Tv shell am start -n dev.tunnelrez.tv/android.app.NativeActivity
}
Write-Host "Logs: adb -s $Tv logcat -s LRT"
