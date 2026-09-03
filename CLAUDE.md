# pulsar-core — shared Rust engine

The headless engine of Pulsar, consumed by `pulsar-desktop` and `pulsar-mobile`
as a **git dependency** (never a path dep). After pushing here, bump consumers
with `cargo update` in their repos. See the umbrella `../CLAUDE.md` for the
product; `README.md` here for the consumer snippet.

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
  delay-gradient estimator), `fec_policy` (parity group size from loss). Simulation matrix
  in `tests/adapt_scenarios.rs`; design in `docs/adaptive-streaming.md`.
- `connection/` — `Node`: register → relay-assigned stable 9-digit ID → P2P
  hole-punch → relay fallback; encrypted `Session`.
- `crypto.rs` — X25519 identity + ChaCha20-Poly1305; per-session salt +
  session_id in the KDF, direction byte in the nonce.
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
  headers (`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`).
- `audio/` — WASAPI loopback (+ per-process), Opus/RTP command builders,
  mute policy (`AudioSettings::policy` — game mode transmits but never
  force-mutes the captured endpoint), endpoint redirect sinks.
- `discovery.rs` — LAN UDP multicast beacon (239.255.71.21), multi-NIC.
- `service/` — the app protocol over the encrypted session: OTP auth,
  keepalive `Ping`/`Pong`/`Bye`, media framing + NACK + **XOR FEC parity**
  (`media.rs`), and **`DataMsg` side channels** (clipboard/chat/file/mic-audio/
  avatar/fs-browse/client `Stats` JSON/…) in `wire.rs`.
  **`DataMsg` lives HERE, not in pulsar-proto** — proto only covers the
  relay/transport layer.

**Division of labor:** this repo has NO real video encoder/decoder. Windows
native capture+encode = `pulsar-desktop/crates/pulsar-capture` (DXGI→NVENC SDK);
rendering = `pulsar-desktop/crates/pulsar-render`; Android encode =
pulsar-mobile's Kotlin plugin. Here: traits, arg builders, and the Linux
Wayland capture.

## Test

```bash
cargo test    # e2e (relay + 2 nodes), streaming, auth races, side channels,
              # disconnect, input service, … (tests/ has 9 files)
```
