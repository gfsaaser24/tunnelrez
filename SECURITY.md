# Security

## Reporting a problem

Please report security problems privately, not in a public issue: use GitHub's **Report a vulnerability** button
(Security tab of this repository, private vulnerability reporting). Say what you found, how to reproduce it, and
which version (`app/Cargo.toml`, or the version in the panel's footer).

## What TunnelRez trusts, and what it does not

- **The LAN.** The TV app accepts one TCP client on port 7878 with no authentication or encryption (PROTOCOL.md).
  Anyone on the same network can connect to the TV app and send it video, or connect to it and end your stream.
  Use TunnelRez on a network you trust. Remote streaming over the internet is not built yet
  ([REMOTE-SPEC.md](docs/REMOTE-SPEC.md) describes the plan, with pairing and per-device keys).
- **adb.** The app uses adb only to open the TV app. Leaving adb over TCP on (port 5555) lets anyone on the network
  who is approved on the TV control it; approve only your own PC.
- **Elevated steps.** Installing the TV speaker or the Parsec driver asks Windows once (UAC). The sender checks the
  bundled Parsec installer against its SHA-256 before it runs it (the installer build also checks its Parsec
  signature). The TV speaker comes from the bundled Microsoft-signed driver files (or a Steam install's copy);
  Windows loads it only with that signature. The bundled files are checked against `drivers/*/SHA256SUMS` by CI and
  by the installer build.
- **The installer** installs for the current user only and needs no admin rights. It is not code-signed yet:
  check `SHA256SUMS` of the release.
- **Logs** never hold IP addresses (they are replaced with `<ip>`), SSIDs or BSSIDs.

## Supported versions

Only the latest release gets fixes.
