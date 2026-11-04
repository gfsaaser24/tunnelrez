# Parsec Virtual Display Driver installer

<!-- PINNED: see docs/PINNED.md (0.45.0.0 and this SHA-256) -->

The virtual monitor TunnelRez shows on the TV. This is the official installer, unchanged, from
`https://builds.parsec.app/vdd/parsec-vdd-0.45.0.0.exe`.

| file | bytes | SHA-256 |
|---|---|---|
| `parsec-vdd-0.45.0.0.exe` | 517256 | `e23332448fdaf5aa017cb308db5ef6855fac526a7ded05d80c039404126d5362` |

Authenticode: valid, signed by "Parsec Cloud, Inc." (DigiCert EV code signing, thumbprint
`ED7876A8EAB9C88F7AFA770A00A4D5AE2A36059B`) and timestamped, so the signature stays valid after the certificate's
own end date.

`SHA256SUMS` holds the same hash. CI (`.github/workflows/drivers.yml`) and the sender's tests (`cargo test`,
`vdd_driver`) check it. The sender also has the hash built in, and checks the file again right before it runs it
elevated, so a changed file never runs. `tools/make-kit.ps1` checks the hash and the signature.

Check it yourself (PowerShell):

```powershell
cd drivers\parsec-vdd
(Get-FileHash parsec-vdd-0.45.0.0.exe -Algorithm SHA256).Hash.ToLower()
Get-AuthenticodeSignature parsec-vdd-0.45.0.0.exe | Format-List Status, SignerCertificate
```

Install: the app's TV monitor row, or `tunnelrez-send vdd install` (one UAC prompt; runs this file with `/S`, then
sets the render-on-NVIDIA values). `tunnelrez-send vdd status` shows the state.

Why the file is in the repo and not a release asset: it is 517 KB and never changes, so it costs almost nothing, and
the kit script, the tests and CI can check it without a network and without a release to depend on. The TV speaker
driver next to it works the same way.
