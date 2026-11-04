# Pinned versions

Every version below is held on purpose. Each site carries a `PINNED: see docs/PINNED.md` comment. Change a pin only in
its own PR, with the build, the tests and (for anything on the stream path) a run-book entry from a real TV.

## Toolchains

| Pin | Value | Set in | Why | What breaks |
|---|---|---|---|---|
| Rust | 1.97.1, components clippy + rustfmt, targets `x86_64-pc-windows-msvc`, `armv7-linux-androideabi`, `aarch64-linux-android` | `rust-toolchain.toml` | Every crate is checked with `clippy -D warnings`; a newer toolchain adds lints and fails the build. The lint table uses `#[expect]`, which needs 1.81+. | New lints turn CI red; an older toolchain misses `expect` and the Android targets. |
| tauri-cli | 2.12.4 (bundler 2.10.4) | `tools/build-installer.ps1` (checked) | `app/installer/hooks.nsh` is written for this NSIS template: the hook macros, `$UpdateMode`, a free `.onGUIInit`, the uninstaller relaunch. | A new template can define `.onGUIInit` (the build fails) or change the upgrade flow (an upgrade could remove the TV speaker). |
| Android Gradle Plugin | 8.5.2 | `receiver/android/build.gradle` | Builds the native-only APK with Gradle 8.9 and JDK 17 (the tested pair). | AGP and Gradle move in pairs: a newer AGP may need a newer Gradle and JDK (unverified which). |
| Gradle | 8.9 (with its SHA-256) | `receiver/android/gradle/wrapper/gradle-wrapper.properties` | The wrapper checks the download. | A mismatched AGP/Gradle pair does not configure. |
| JDK | 17 | `receiver/build-and-install.ps1` (Eclipse Adoptium `jdk-17*`, or `JAVA_HOME`) | AGP 8.x needs JDK 17. | JDK 11 does not run AGP 8; JDK 21+ is untested. |
| compileSdk / targetSdk | 34 | `receiver/android/app/build.gradle` | The SDK platform the build has (`platforms;android-34`). | A missing platform stops the build. |
| minSdk | 26 | `build.gradle`, `receiver/Cargo.toml` (`ndk` feature `api-level-26`), `cargo ndk -P 26` | AAudio and `AMediaCodec` need 26. The three must agree. | A lower value links symbols the TV does not have. |
| TV API | 29 or newer on the TV | `receiver/src/wifi.rs`, `caps.rs`, `decoders.rs` | The low-latency Wi-Fi lock, `isHardwareAccelerated` and performance points are API 29. On 26-28 the app runs without them. | Nothing on 29+; on 26-28 Auto has fewer decoder facts. |
| NDK | 29.0.14206865 | `build.gradle` (`ndkVersion`), `receiver/build-and-install.ps1` | The NDK the TV app was tested with. `ANDROID_NDK_HOME` overrides it. | Another NDK may change the C runtime the `.so` links against. |

## Crates (exact versions; `Cargo.lock` is committed for every binary crate)

| Crate | Version | Set in | Why | What breaks |
|---|---|---|---|---|
| `ndk` / `ndk-sys` | =0.9.0 / =0.6.0 | `receiver/Cargo.toml` | ndk 0.9 maps every non-zero AAudio result to `Err`, but `write` and `set_buffer_size_in_frames` return a frame count. `aaudio_count` in `receiver/src/audio.rs` turns that "error" back into the count. | A fixed ndk returns `Ok(count)` and the workaround must go; a different bug shape reopens the stream after every write (choppy sound, PR #30). |
| `rtrb` | =0.4.1 | `receiver/Cargo.toml` | The wait-free ring between the UDP thread and the AAudio callback; the callback never waits. | An API change breaks the audio path. |
| `windows` / `windows-core` | =0.61.3 / =0.61.2 | `sender/Cargo.toml`, `native/Cargo.toml` | The sender and the native backend pass COM and D3D11 types to each other: one version for both. | Two versions: types do not match between the crates. |
| `windows` | =0.62.2 | `app/Cargo.toml` | The same major as Tauri's own, so one copy is linked. | A second copy, or type mismatches with `webview2-com`. |
| `windows` | =0.58.0 | `vdd/Cargo.toml` | The Parsec control code (`DeviceIoControl`, SetupAPI) is written against 0.58. | API renames between majors. |
| `tauri` / `tauri-build` | =2.12.5 / =2.7.4 | `app/Cargo.toml` | The panel window (acrylic, no decorations, tray) and the installer hooks were checked on these. | Window behavior and the bundle config can shift between minors. |
| `webview2-com` | =0.39.1 | `app/Cargo.toml` | The same version `wry` uses; only for the hidden-panel memory hint. | Two copies of the WebView2 bindings. |
| `socket2` | =0.6.5 | `sender/Cargo.toml` | The video socket: 2 MiB send buffer, `TCP_NODELAY`, connect timeout (`sender/src/session.rs`). | A new major can rename these calls (unverified); the tested behavior is this version's. |
| `quinn` | not used | - | - | - |

## Outside the repo

| Pin | Value | Set in | Why | What breaks |
|---|---|---|---|---|
| NVENC API | 13.1 (Video Codec SDK 13.1; headers from nv-codec-headers `n13.1.15.0`) | `native/src/nvenc/sys/` (`regen.sh`, `api.rs`, `version.rs`) | The struct versions in `version.rs` must match the driver's API. | Regenerating against another header changes every struct version. |
| NVIDIA driver | 610 or newer | `native/src/nvenc/mod.rs` (`MIN_DRIVER`) | API 13.1 needs it (SDK read-me). | An older driver: native NVENC stops with a clear error and `--encoder auto` falls back to FFmpeg. |
| Parsec VDD | 0.45.0.0, SHA-256 `e23332448fdaf5aa017cb308db5ef6855fac526a7ded05d80c039404126d5362`, signed "Parsec Cloud, Inc." | `drivers/parsec-vdd/` (README, SHA256SUMS), `sender/src/vdd_driver.rs` (`VERSION`, the hash) | The control interface (IOCTLs, the 1 s keep-alive) is written for this driver; the installer runs elevated, so it must be this exact file. | Another version may change the IOCTLs; another file fails the hash check (CI, the installer build, the sender). |
| Steam Streaming Speakers | the files below, `.cat` signed by Microsoft (WHQL), `.sys` by Valve | `drivers/steam-streaming-speakers/` (README, SHA256SUMS) | A kernel driver: it loads only with a Microsoft signature, so the files must be the signed originals. | A changed byte breaks the signature: Windows refuses the driver. |
| FFmpeg | 7 or newer on PATH; 8 or newer for NVENC padding (`-cbr_padding`) | `README.md`; padding detected in `sender/src/tools.rs` and `encoder.rs` | The FFmpeg backend is the fallback. Without `-cbr_padding` NVENC rates are a ceiling, not fixed. FFmpeg 7 is documented, not checked; 7.1 and 8.1 are the tested versions. | FFmpeg 6 and older are untested; FFmpeg 7.x streams with a rate ceiling. |

Steam Streaming Speakers SHA-256 (`drivers/steam-streaming-speakers/SHA256SUMS`):

```
996fe11be464a2be518e4cbab5c46fc7a250c6a67cf705a75ce3bb7b2f2611f4  x64/SteamStreamingMicrophone.inf
0ec2de32da3afbc63ea84e76d1f0322382c273b3162885c7254f3960802b3b5e  x64/SteamStreamingMicrophone.sys
40899fad06a5f0b23f202e96dec9fed3b7fa38979eeeb5f62613b172e927203b  x64/SteamStreamingSpeakers.inf
ee1ee0d1346328da18d70988f01bd961437e95be5ab82059a28cbcc7d4b23239  x64/SteamStreamingSpeakers.sys
a767b5f76a5f9eddde5793b81370280adeddd4658e164798dcd451ede7e1a212  x64/steamstreamingmicrophone.cat
97a03a1567e6913eb7972f96f98fb0284e41d10be1de13a846a1a529c2d4bfc1  x64/steamstreamingspeakers.cat
```

## The product version

Not a pin, but one place: `version` in `app/Cargo.toml`. Tauri (the app and the installer) and the TV app's
`versionName`/`versionCode` (`receiver/android/app/build.gradle`) read it. `sender/Cargo.toml` must say the same;
`tools/build-installer.ps1` stops when it does not.
