# Steam Streaming Speakers driver

<!-- PINNED: see docs/PINNED.md (these hashes are checked by CI, the installer build and the sender) -->

The virtual playback device TunnelRez streams PC sound through (see `PROTOCOL.md`, "Virtual sink"). These files
are copied unchanged, byte for byte, from a Steam install
(`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\x64\`). The `.cat` catalogs are signed by
"Microsoft Windows Hardware Compatibility Publisher" and the `.sys` files by Valve, so the driver loads with
Secure Boot on and without test mode.

| file | bytes | SHA-256 |
|---|---|---|
| `x64/SteamStreamingSpeakers.inf` | 3718 | `40899fad06a5f0b23f202e96dec9fed3b7fa38979eeeb5f62613b172e927203b` |
| `x64/SteamStreamingSpeakers.sys` | 40736 | `ee1ee0d1346328da18d70988f01bd961437e95be5ab82059a28cbcc7d4b23239` |
| `x64/steamstreamingspeakers.cat` | 10722 | `97a03a1567e6913eb7972f96f98fb0284e41d10be1de13a846a1a529c2d4bfc1` |
| `x64/SteamStreamingMicrophone.inf` | 3818 | `996fe11be464a2be518e4cbab5c46fc7a250c6a67cf705a75ce3bb7b2f2611f4` |
| `x64/SteamStreamingMicrophone.sys` | 40736 | `0ec2de32da3afbc63ea84e76d1f0322382c273b3162885c7254f3960802b3b5e` |
| `x64/steamstreamingmicrophone.cat` | 10748 | `a767b5f76a5f9eddde5793b81370280adeddd4658e164798dcd451ede7e1a212` |

Only the speakers are installed. The microphone set is kept for later and is not used.

`SHA256SUMS` holds the same hashes. CI (`.github/workflows/drivers.yml`) and the sender's tests
(`cargo test`, `audio::driver`) check them. `.gitattributes` marks this folder `-text`, so git never changes the
line endings of the `.inf` files.

Check the hashes and the signatures (PowerShell):

```powershell
cd drivers\steam-streaming-speakers
Get-Content SHA256SUMS | ForEach-Object { $h, $f = $_ -split '\s+'; "{0}  {1}" -f ((Get-FileHash $f -Algorithm SHA256).Hash.ToLower() -eq $h), $f }
Get-ChildItem x64\*.cat, x64\*.sys | Get-AuthenticodeSignature | Format-Table Status, @{n='Signer'; e={$_.SignerCertificate.Subject.Split(',')[0]}}, Path
```

Or with the Windows SDK: `signtool verify /kp /v /c x64\steamstreamingspeakers.cat x64\SteamStreamingSpeakers.sys`.

Install: `tunnelrez-send audio-sink install` (one UAC prompt; the app's "Install TV speaker" button runs it).
The INF's `[DeviceInstall32]` section creates the root device `ROOT\SteamStreamingSpeakers\0000`.
`tunnelrez-send audio-sink status` shows the state; `audio-sink uninstall` removes a device that TunnelRez
installed.
