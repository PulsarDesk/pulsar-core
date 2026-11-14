# Workspace + modules

Part of the pulsar-core agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Workspace

- `crates/pulsar-core` — the engine (the only workspace **member**).
- `crates/vigem-client` — vendored fork of vigem-client 0.1.4 (+ DS4 extended
  report). **NOT a workspace member** (Windows-only code); wired via
  `[patch.crates-io] vigem-client = { path = … }`. Consumers re-patch it by git
  URL to this same repo.
- Deps on `pulsar-proto` (git) and `pulsar-relay` (git, dev-dependency for tests).

## Modules (`crates/pulsar-core/src/`)

- `adapt/` — **adaptive streaming controller** shared by both apps (pure, no I/O):
  `Controller` (2 s windows → target rate, operating point, encoder bitrate net of FEC,
  loss-recovery mode), `ladder` (resolution × fps rungs per codec), `delay` (trendline
  delay-gradient estimator over arrival **groups**, WebRTC InterArrival style), `fec_policy`
  (v1 `parity_ratio` for mobile + v2 `FecPolicy` sized from pre-repair loss, desktop host).
  Simulation matrix in `tests/adapt_scenarios.rs`; design in `docs/adaptive-streaming.md`
  (poor-link rules of 2026-11-09: cautious 4 Mbit start, soft pins, point floor, goodput cut,
  floor escape — `Sample::recv_bytes`/`window_ms` > 0 enables the goodput paths; mobile
  passes 0 and keeps the old halving).
- `connection/` — `Node`: register → relay-assigned stable 9-digit ID → P2P
  hole-punch → relay fallback; encrypted `Session`. Punching: opening burst + retransmits for
  `PUNCH_TIMEOUT`, then the session starts on the relay while BOTH ends keep punching for
  `BACKGROUND_PUNCH` (20 s) — the Punch/PunchAck handlers move it to `Direct` when one
  answers. A punch from the peer's public IP on a NEW port is accepted as a peer-reflexive
  candidate until the path is proven (mobile carrier NATs map per destination).
  `connection/portmap.rs`: on the first `register()` the node maps its UDP port on the router
  (NAT-PMP inline, UPnP-IGD in the background; renewed every 10 min, deleted on drop; skipped
  for loopback nodes / relay-only / `Config::port_mapping = false`). Measured 2026-11-08: a
  home router behind the ISP's CGNAT let 0 punch packets through without it, all with it.
  `connection/path.rs` (2026-11-09, Auto mode): **path watchdog** — a relay-rendezvous session
  on Direct whose direct path is silent 3 s while the relay answers a probe falls back to the
  relay without dropping (`live_transport()` → Relay; `transport()` stays the connect-time
  snapshot); authenticated relay Data after ≥ 1 s direct silence = the peer moved → follow at
  once. Re-upgrade to direct after a hold-down (10 s, doubled per fallback, ≤ 160 s); while
  relayed, `punch_in_background` re-punches every 15 s for the whole stint; a direct session
  sends one relay-tunnelled probe per 30 s so the relay keeps its session entry. Only
  adoption / probe-range acks stamp direct liveness (a peer's background punches prove one
  direction only). **NAT rebinding:** an authenticated, non-replayed packet from a new address
  after 1 s quiet on the old one triggers a Punch path check; only the matching PunchAck moves
  `peer_addr` (residual: the check is unauthenticated — real PATH_CHALLENGE needs a wire change).
- `crypto.rs` — X25519 identity + ChaCha20-Poly1305; per-session salt +
  session_id in the KDF, direction byte in the nonce. **Key v3** adds an ephemeral X25519 DH (forward
  secrecy) when the peer sends one — relay blob extension `ver||caps||eph` after the
  always-written 6-byte LAN slot, direct-IP `Hello2/HelloAck2`; old peers → v2 key.
  `Session::binding()` = channel binding (differs on MITM legs).
- `config.rs` — persisted JSON config; `DEFAULT_RELAY = "relay.pulsardesk.com"` — the
  official public relay, written without a port because 21116 is implied when an address
  carries none (users can point at a self-hosted/local one, with or without a port);
  `NetworkMode`; `relay_keys` holds the access key each relay issued this device.
- `input/` — controller types/normalization + virtual pads: uinput (Linux),
  ViGEmBus X360+DS4 (`vigem.rs`), Win32 `SendInput`, per-window `PostMessage`
  (co-op), macOS CGEvent, DS4/DS5 touchpad-as-mouse. **Controller *reading*
  moved to SDL3 in the desktop app — gilrs is gone** (ignore stale doc comments
  saying otherwise). macOS virtual pad = no-op stub.
- `media.rs` — capture→encode→transport→decode traits + RLE software baseline
  **for tests only**. Real encoders live in the apps (see below).
- `pipeline/` — **pure** ffmpeg/gst arg builders + `HwEncoder`/`VCodec`
  detect/resolve. No process spawning here (that's the Tauri layer).
- `capture.rs` — **Linux-only** (`cfg`): Wayland XDG ScreenCast portal →
  PipeWire → an **in-process GStreamer pipeline** (gstreamer-rs; live `set_bitrate`,
  `request_keyframe`, `set_short_gop`); `is_wayland()`. Build needs the GStreamer dev
  headers (`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`). If the in-process
  GStreamer lacks an element of the pipeline, the same description runs under the system
  `gst-launch-1.0` (fd inherited; no live controls, `is_in_process() == false`);
  `use_system_gst_env` strips an AppImage's bundled-GStreamer env (incl. `$APPDIR` entries of
  `LD_LIBRARY_PATH`/`PATH` — else the "system" tool is, or loads, the bundled one) from such children.
  `start(.., system_gst)` forces that child (caller validated the encoder only on the
  system gst). `self_check(fragment)` = headless in-process encode test (desktop `--selftest`).
  gst VA-API = two families, one wire id `vaapi`: `Vaapi` (gstreamer-vaapi, `vaapih264enc`)
  and `Va` (gst-plugins-bad `va`, `vah264enc`, what gst ≥1.26 distros ship).
- `macos_capture/` — **macOS ScreenCaptureKit capture** (2026-11-14, untested on macOS
  hardware): `sck::ScStream` (one display: NV12 frames with the cursor at the client's fps,
  and/or the system audio without this process — macOS 13+), `displays()` (main first = the
  host's display index), `screen_capture_access(prompt)` (TCC Screen Recording). ScreenCaptureKit
  is **dlopen'ed, never linked** (objc2 runtime only, no objc2-* framework crates — they link
  strongly and the app would not start on macOS < 12.3); newer constants via `dlsym`.
  `pcm.rs` (planar → interleaved stereo) is pure and tested everywhere. The encoder that eats
  the frames is in the desktop app (`pulsar-capture/src/mac/vt.rs`, VideoToolbox). Frames are
  stretched to the output size (`preservesAspectRatio = NO`), never letterboxed.
  `set_streamed_display` / `streamed_display` publish the display being streamed (process-wide)
  so `input/macos.rs` maps the absolute pointer onto it instead of always the main display;
  `avfoundation_screen_number` gives the ffmpeg fallback's `"Capture screen N"`.
- `audio/` — WASAPI loopback (+ per-process), Opus/RTP command builders,
  mute policy (`AudioSettings::policy` — game mode transmits but never
  force-mutes the captured endpoint), endpoint redirect sinks. macOS: `sck_loopback.rs`
  (`run_system_audio_capture`, the system mix via ScreenCaptureKit — the counterpart of the
  Windows process loopback in `Exclude` mode); `Config::audio_loopback()` = "system mix" on
  Windows and macOS when no device is named.
- `discovery.rs` — LAN UDP multicast beacon (239.255.71.21), multi-NIC.
- `service/` — the app protocol over the encrypted session: OTP auth (non-empty
  passwords via **SPAKE2 PAKE** in `service/pake.rs`, MAC'd over the channel binding —
  the password never travels; new clients refuse to send one to an old host
  → `ConnError::PeerOutdated`; hosts still accept legacy plaintext from old clients),
  keepalive `Ping`/`Pong`/`Bye` (no ping id — the desktop client's `PongMatcher` copes),
  media framing + NACK + **FEC parity** — Reed-Solomon (`TAG_FEC_RS`; `RsFecEncoder::new`
  groups up to 3 small frames within 25 ms into one block, `per_frame` = old behaviour,
  released decoders unchanged), XOR kept for v0.11.0 clients, `MediaSendStats` send counters
  (`media.rs`), and **`DataMsg` side channels** (clipboard/chat/file/mic-audio/
  avatar/fs-browse/client `Stats` JSON/…) in `wire.rs`.
  **`DataMsg` lives HERE, not in pulsar-proto** — proto only covers the
  relay/transport layer.

**Division of labor:** this repo has NO real video encoder/decoder. Windows
native capture+encode = `pulsar-desktop/crates/pulsar-capture` (DXGI→NVENC SDK);
rendering = `pulsar-desktop/crates/pulsar-render`; Android encode =
pulsar-mobile's Kotlin plugin. Here: traits, arg builders, and the Linux
Wayland capture.
