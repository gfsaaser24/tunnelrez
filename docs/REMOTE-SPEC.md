# TunnelRez Remote: stream to any TV, anywhere

Status: spec, not built (coming). Branch `dev`. The product was called SlipStream before; it is now **TunnelRez**
(site: tunnelrez.com), and the code uses the new names (`tunnelrez-send`, `dev.tunnelrez.tv`).

## 1. Goal

Stream a PC to a TunnelRez TV that is **not on the same network**, set up by **two people in two places** with no
local pairing step:

- Person A is at the TV (for example a parent's house). They set the TV up once.
- Person B is at the PC, anywhere. They see the TV waiting and press Connect.

The user picks how to connect. Local streaming (today) stays the default on one network.

Non-goals: remote control of the PC from the TV (no input), many viewers of one stream, recording in the cloud, any
server that we (the TunnelRez project) run. Every user runs their own relay in their own Cloudflare account.

## 2. Connection modes (user picks, per TV)

| Mode | Path | Needs the relay Worker | When |
|---|---|---|---|
| **Auto** (default) | Local, then Direct, then Relay | Only after Local fails | Most users |
| **Local** | Same network, as today | No | Home |
| **Direct** | Internet, device to device (UDP hole punching) | Yes, for pairing and addresses | Most home routers |
| **Relay** | Always through Cloudflare Realtime TURN | Yes | Strict networks: hotels, offices, CGNAT on both ends |
| **Tailscale** | The user's own tailnet: connect to the TV's tailnet address | No | Users who already run Tailscale |

Auto reports which path it took (Local / Direct / Relay) in the PC app and on the TV, with the measured round trip.

## 3. Parts

```
 TV (Android)                 Owner's Cloudflare account                      PC (Windows)
┌──────────────┐  WebSocket  ┌───────────────────────────────────────┐  WebSocket  ┌──────────────┐
│ TunnelRez TV │◀──────────▶ │ TunnelRez Relay Worker (template)     │ ◀─────────▶ │ TunnelRez PC │
│ app          │  presence,  │  ├─ Room: Durable Object per owner    │  presence,  │ app + sender │
│              │  handshake  │  │    (presence, handshake, invites) │  handshake  │              │
│              │             │  ├─ D1: devices, keys, invites, log   │             │              │
│              │             │  ├─ Admin page (behind CF Access)     │             │              │
│              │             │  └─ TURN credential minting           │             │              │
│              │             └───────────────────────────────────────┘             │              │
│              │◀════════ QUIC over UDP: direct, or via Cloudflare Realtime TURN ═══▶│              │
└──────────────┘        (video, audio, stats; end-to-end encrypted, never through the Worker)
```

- **Relay Worker**: a template the owner deploys ("Deploy to Cloudflare" button in the README, or
  `wrangler deploy`). Holds the TURN key as a secret. Carries only small JSON messages, never media. Workers cannot
  relay UDP, and media must not depend on Worker CPU limits.
- **Room**: one Durable Object per owner (one Worker = one owner). Keeps a WebSocket per online device using
  hibernatable WebSockets, so idle TVs cost almost nothing.
- **D1**: the device list, public keys, invites, PC tokens, an event log.
- **TURN**: Cloudflare Realtime TURN. The Worker mints short-lived credentials per session
  (`POST https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers`, body `{"ttl":…}`,
  bearer = the TURN API token secret). STUN: `stun.cloudflare.com:3478` (free).

## 4. Setup: two people, two places

### 4.1 Owner: deploy the relay (once)

1. Deploy the Worker template to their Cloudflare account.
2. Create a TURN key in the Cloudflare dashboard (Realtime → TURN). Paste its id and API token into the Worker's
   secrets (`TURN_KEY_ID`, `TURN_KEY_API_TOKEN`). The admin page has a setup checklist that tests both.
3. Put the admin page behind Cloudflare Access (free tier). The template includes the Access setup steps. The
   device and handshake endpoints stay public but every call is signed (section 6).

The owner is often Person B, but does not have to be.

### 4.2 Add a TV (Person A, at the TV, no PC on that network needed)

1. Owner, on the admin page: **Add TV** → name ("Mom's living room") → the page shows a one-time **invite**:
   8 characters (`K7QM-4R2X`, no look-alike letters), valid 24 h, one use. Plus a copy-ready command:

   ```
   adb shell am start -n <tv-app>/.Setup --es relay https://<worker-host> --es invite K7QM-4R2X
   ```

2. Person A sets the TV up by **either**:
   - **ADB**, from any computer on the TV's network: run the command above. (The TV app reads intent extras, not
     environment variables or system properties: those are not readable or not persistent.)
   - **Remote only**: open the TunnelRez TV app → Setup → type the relay address and the invite with the remote.
     The address field accepts a short custom domain (`tv.example.com`), so typing stays short.
3. The TV app:
   1. Makes its own key pair (Ed25519 identity; stored in the app's private storage, Android Keystore where the
      TV supports it).
   2. Calls `POST /v1/enroll` with the invite and its **public** key. The Worker checks the invite (unused,
      unexpired), binds it to this key, marks it used, and returns the device id and the relay settings.
   3. Saves the config in app storage (survives reboots and updates).
   4. Opens the Room WebSocket and shows **"✓ Connected to relay — waiting for a PC"** with its name.
4. A failed step shows a red ✕ and the reason in plain words: "invite expired", "relay not reachable",
   "no internet", "clock wrong" (signed requests need a sane clock).

A TV can be set up while its owner is away: Person A only needs the invite and the address.

### 4.3 Add a PC (Person B, anywhere)

1. Owner, on the admin page: **Add PC** → name → a one-time PC token (same rules as invites).
2. In the TunnelRez PC app: Remote → paste the relay address and the token. The PC makes its own key pair and
   enrolls the same way (`POST /v1/enroll`, role `pc`).
3. The PC app shows the relay as connected and lists the owner's TVs with state:
   **waiting** (online, free), **busy** (streaming to another PC), **offline** (last seen …).

## 5. Connecting

1. Both devices hold an open WebSocket to the Room and send a heartbeat (`ping` every 20 s; the Room marks a device
   offline after 60 s without one). The TV stays "waiting" until a PC connects. This is the "I'm ready" signal.
2. Person B presses **Connect** on "Mom's living room".
3. **Consent (default on for remote TVs):** the TV shows "Alex's PC wants to stream to this TV. OK / No" and Person
   A answers with the remote. Owners can turn this off per TV. No answer in 60 s = no.
4. The Room runs the handshake:
   1. Mints TURN credentials for both sides (TTL = 1 h, renewed during long sessions).
   2. Sends each side the other's public key and a fresh session id.
   3. Relays candidates both ways: host (LAN) addresses, server-reflexive (from STUN), relay (from TURN).
5. The devices test the candidate pairs (ICE-style: local first, then direct, then relay; or only the mode the user
   forced) and keep the first pair that completes the QUIC handshake. Typical: under 2 s.
6. The Worker steps out. Media flows device to device or through TURN. The Room only hears "session started" and
   "session ended" (with duration and path, for the admin log).

### 5.1 Room messages (JSON over WebSocket, every message signed, section 6)

| Type | From → to | Content |
|---|---|---|
| `hello` | device → Room | device id, role, app version, capabilities (codecs, max size, decoders) |
| `presence` | Room → PCs | list of TVs with state |
| `connect` | PC → Room | target TV id, wanted mode, wanted picture (size, fps, codec, bitrate ceiling) |
| `consent` | Room → TV, TV → Room | the request; ok / no |
| `session` | Room → both | session id, peer public key, peer capabilities, TURN servers + credentials |
| `candidate` | device → Room → device | one transport candidate (type, address, priority) |
| `selected` | device → Room | chosen path (local / direct / relay), RTT |
| `end` | either → Room | reason |

## 6. Security

- **Keys made on the device.** Only public keys reach the Worker. A leaked Worker database leaks no stream keys.
- **Every Room message and enroll call is signed** with the device key (Ed25519, with a timestamp and nonce). The
  Room rejects replays and unknown keys. Removing a device on the admin page revokes it immediately.
- **The stream is end-to-end encrypted:** QUIC (TLS 1.3) with certificates pinned to the two device keys exchanged
  in `session`. Neither the Worker nor Cloudflare TURN can read it. A device that presents another key is
  rejected.
- **Invites and PC tokens:** one use, 24 h expiry, rate-limited enroll (per IP and per Worker). Wrong invites are
  logged.
- **Consent on the TV** (default on for remote TVs) stops a stolen PC token from silently streaming into someone's
  living room.
- **Admin page behind Cloudflare Access.** The template refuses to show the admin page if Access is not set up
  (it checks the `Cf-Access-Jwt-Assertion` header).
- **No secrets in the repo.** The TURN key lives only in the owner's Worker secrets.

## 7. Transport: QUIC over UDP (replaces TCP for remote; also offered on the LAN)

Why: TURN relays UDP; the internet needs encryption and congestion control; QUIC gives both, plus datagrams.

- Rust on both ends: `quinn` (PC) and `quinn` built for armv7 Android (TV).
- **Video:** one access unit per QUIC stream, or frames split into datagrams with a small FEC and an in-frame
  resend window (to be measured; see the research notes). A late frame is dropped, never queued.
- **Audio:** QUIC datagrams, 5 ms PCM as today (or Opus at low rates on slow links, measured first).
- **Control and stats:** one bidirectional stream (today's STATS, IDR_REQUEST, CAPS as messages).
- **Through TURN:** the device speaks TURN itself (allocation + ChannelData) and runs QUIC inside it. Candidate:
  the `turn` crate from webrtc-rs, or a small own client (the protocol part needed is small).
- **Adaptive bitrate is required** on remote paths: the control loop (brain spec, PR #8) drives the encoder from
  QUIC's congestion signals (RTT, loss, delivered rate). Starting points: 1080p60 HEVC 8–15 Mb/s; 4K only on fast
  uplinks.

## 8. Admin page (owner only, behind Access)

- Devices: name, role, state (online/offline/busy), last seen, app version, path of the last session. Rename,
  remove (revokes), require consent on/off.
- Invites and PC tokens: create, list, cancel.
- Sessions: start, end, duration, path (direct / relay), bytes relayed.
- TURN usage: relayed GB this month vs the free 1,000 GB, from Cloudflare's analytics.
- Setup checklist: TURN secrets present and valid, Access on, a test connection.

## 9. The apps

- **TV app:** a Remote screen with the relay status (✓ / ✕ + reason), the TV's name, "waiting" / "streaming from
  …", the consent prompt, and Setup (enter relay + invite with the remote). Works with or without ADB.
- **PC app:** a Remote section: relay status, the TV list with state and a Connect button per TV, the chosen mode
  per TV, and the path and RTT while streaming. Local TVs keep working with no relay.

## 10. Cost to the user

- Workers free plan: Room, D1 and the admin page fit easily (a handful of messages per session).
- Cloudflare Realtime TURN: $0.05 per GB sent from Cloudflare, first 1,000 GB per month free (shared with the SFU).
  1080p60 at 10 Mb/s ≈ 4.5 GB/h → about 220 relayed hours a month free. Direct and Local cost nothing.

## 11. Build order (each a PR to `dev`)

1. **Rename to TunnelRez** (app, sender, TV package, installer, settings folder with migration, docs).
2. **QUIC transport on the LAN** behind a setting, measured against today's TCP in the run book.
3. **Relay Worker template:** Room, D1, enroll, presence, admin page, TURN minting, Deploy button, tests.
4. **TV setup and status screens**, ADB intent and remote entry.
5. **Direct mode:** STUN, candidates, hole punching.
6. **Relay mode:** TURN client + QUIC through TURN.
7. **Adaptive bitrate for remote paths** (with the brain).
8. **Tailscale mode** (a tailnet address as a target).

## 12. Acceptance tests

- A TV set up with only an invite and the relay address (no PC on its network) shows ✓ and appears as waiting.
- A PC on another network connects; the TV asks for consent; streaming starts in under 5 s from Connect.
- With UDP between the sites blocked except to Cloudflare, Auto falls back to Relay and keeps streaming.
- Removing the TV on the admin page ends a running session within 5 s and blocks reconnects.
- A replayed or unsigned Room message is rejected.
- The admin page is unreachable without Access.
- Remote 1080p60 on a 20 Mb/s uplink: no stall over 2 s in a 10-minute run (adaptive bitrate on).

## 13. References

- RustDesk (`rustdesk/rustdesk`, `rustdesk/rustdesk-server`): rendezvous server (hbbs) + relay (hbbr), ID pairing,
  hole punching then relay fallback. The same model, self-hosted; we copy the model, not the code.
- `cloudflare/meet` (Orange Meets) and `cloudflare/partykit`: Worker + Durable Object signaling and the TURN
  credential call.
- `mazupo/cf-sfu-worker`: devices waiting in a Worker room for a team to connect.
- Cloudflare docs: Realtime TURN (pricing, FAQ), Durable Objects WebSocket hibernation, D1, Access.

## 14. Open questions

- TURN over TCP/TLS 443 as a last resort for networks that block UDP entirely (Cloudflare TURN offers it; QUIC over
  it adds head-of-line delay).
- Keep the TV reachable while the app is in the background (a foreground service) or only when it is open.
- One Worker per owner (simple) vs. one Worker shared by a family with several owners (roles).
- Whether to also offer the Realtime SFU (WHIP/WHEP) for H.264-only clients such as a browser viewer.
