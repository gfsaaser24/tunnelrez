# tunnelrez-vdd

Plugs one virtual monitor for the TV through the Parsec Virtual Display Driver, keeps it alive, and unplugs it on
exit. Control interface after [nomi-san/parsec-vdd](https://github.com/nomi-san/parsec-vdd) `core/parsec-vdd.h`.

- Driver: Parsec VDD 0.45 (`https://builds.parsec.app/vdd/parsec-vdd-0.45.0.0.exe`, signed "Parsec Cloud, Inc.",
  SHA-256 `E23332448FDAF5AA017CB308DB5EF6855FAC526A7DED05D80C039404126D5362`), installed 2026-10-25 with `/S`.
  Fallback if it misbehaves: 0.41 ("stable", IddCx 1.4). Uninstall: Apps > "Parsec Virtual Display Driver".
  The installer is bundled in `drivers/parsec-vdd/`; `tunnelrez-send vdd status|repair|install` looks after it.
- Windows attaches a new monitor by itself only when it remembers this set of screens (a new screen such as a USB
  display starts a new set). Otherwise the monitor stays detached and shows up on every idle output of the adapter
  (16 GDI names). `ensure_mode` attaches it right of the rightmost screen, so no other screen moves.
- After a display is removed the driver refuses a new one for about 3 s (`ERROR_GEN_FAILURE`, at once): `plug` retries.
- The driver unplugs every display it added ~1 s after pings stop, so a crashed sender never leaves a stray monitor.
- One virtual monitor only: multi-monitor setups are where every IddCx driver gets unstable.

```
cargo build --release
target\release\tunnelrez-vdd.exe run 1920x1080@60    # plug, extended right of your screens; Ctrl+C unplugs
target\release\tunnelrez-vdd.exe run 1920x1080@60 --hold 10    # the same, unplugged after 10 s
target\release\tunnelrez-vdd.exe list                # the plugged displays, attached or detached
target\release\tunnelrez-vdd.exe mode \\.\DISPLAY10 3840x2160@60    # attach and set a plugged display (the reuse path)
target\release\tunnelrez-vdd.exe unplug-all          # also a display another process keeps alive
```

Tested 2026-10-25: plugged as `\\.\DISPLAY10`, 1920×1080@60 at (2560,0); a hard kill unplugged it within 3 s.
Preset modes include 1920×1080 at 24/30/60/144/240 Hz; 120 Hz would need a custom mode under
`HKLM\SOFTWARE\Parsec\vdd`.
