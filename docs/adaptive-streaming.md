# Adaptive streaming — design and plan

**Status:** approved design. **Phases 0–4 implemented locally on 2026-09-03 — NOT pushed,
awaiting the maintainer's real-session test** (see [Implementation status](#implementation-status)).
**Owner:** maintainer. **Written:** 2026-09-03, from a live diagnosis (below).

## Goal (the maintainer's words, paraphrased)

- A session may take a moment to settle when it **starts** — a short probe is acceptable.
- **Mid-stream, the picture must never freeze.** The client measures the path
  continuously while streaming and adapts automatically; nobody touches a menu.
- This applies to **every encoder, every decoder and every transport** — LAN direct,
  P2P hole-punched, relay-forwarded.
- Target: the **minimum bandwidth the path allows, with the picture as clean as
  possible at that budget** — "cam gibi". Fewer clean pixels beat many broken ones.

## Evidence — why today's behaviour fails (session of 2026-09-03)

Client: this Linux desktop (`hold.rs` controller, `pulsar-render` + CUDA decode).
Host: a friend's Windows PC, AMD (AMF encoder), 1920×1080, HEVC, over the internet,
rendezvous via `relay.pulsardesk.com`. Facts from the client log and the relay log:

| Observation | Meaning |
| --- | --- |
| kernel UDP drops on every socket = 0 | not a local socket/CPU problem |
| loss 24–40 % at 15 Mbit, ~0 % at 2 Mbit | the host's **upload ≈ 2–2.5 Mbit** is the ceiling |
| controller: 2000→2500→9 % loss→2000, every ~20 s | it **re-probes above a known-bad ceiling** and causes the loss it then reacts to (sawtooth) |
| `RTP: dropping old packet received too late` ×182 678 | NACK retransmits land after the renderer's 100 ms `max_delay` (RTT > 100 ms) — **the only repair path is useless** on this route |
| `Could not find ref with POC …` ×16 400 | HEVC reference chain broken → **frozen/smeared until the next keyframe** |
| remote-mode GOP = fps×2 (~2 s) | every loss event = **up to 2 s of freeze** |
| stream stayed 1920×1080 at 2 Mbit | far too few bits per pixel: blocky AND fragile |

Conclusion: adaptation *converges* on bitrate, but (a) it never leaves 1080p, (b) it keeps
re-probing and re-losing, and (c) with a 2 s GOP, no intra-refresh and a dead NACK path,
each loss becomes a multi-second freeze instead of a brief softening.

## What exists today (file references — read before changing anything)

- **Desktop client controller:** `pulsar-desktop/src-tauri/src/play/hold.rs`.
  2 s keepalive window; loss % from RTP sequence gaps (`win_recv`/`win_lost`);
  NACK for small gaps (`MediaNack(Vec<u16>)`); keyframe request = `MediaNack([0])`,
  rate-limited to 1/s; RTT measured via Ping/Pong and emitted as `play-rtt` — **but the
  controller ignores it**; actuates **bitrate only** (floor `ADAPT_MIN_KBPS = 2000`),
  with a `loss_ceiling` memory; every change re-issues a `StreamReq`.
- **Mobile client controller:** `pulsar-mobile/mobile/src/client.rs` (~line 378) — a
  **divergent copy** (floor 3000, uses RTT excess 35/90 ms, ramps after 2 clean s).
  Two implementations, already drifting. Must become one.
- **Host actuation:** NVENC path (`pulsar-desktop/crates/pulsar-capture`) reconfigures
  bitrate **live** (`set_bitrate` → `reconfigure_bitrate`) and forces an IDR on a keyframe
  request. ffmpeg / gst paths **restart capture** on any `StreamReq`, and — worse — the
  ffmpeg path answers a keyframe request by **restarting capture** (`host.rs` on_nack).
- **Encoder GOP:** `pulsar-core/.../pipeline/command.rs` — remote = fps×2, game = fps/4.
  **No intra-refresh, no LTR, no FEC anywhere** (grep confirms).
- **Receiver:** `pulsar-desktop/crates/pulsar-render/src/video.rs` — `reorder_queue_size`
  512, `max_delay` 100 ms (fixed), `discardcorrupt`; no concealment: a frame with missing
  references is decoded and shown as smear.
- **Transport:** `Session.transport` is `Direct`/`Relay`; the relay's per-session
  `rate_cap_kbps` arrives in `PeerFound` (`connection/handlers.rs`) — **not consumed** by
  any controller.
- **Wire:** session messages (`StreamReq`, `StreamCaps`, `DataMsg`) are **serde_json** —
  appending a field with `#[serde(default)]` is additive-safe; an unknown `DataMsg` variant
  is dropped by old receivers (see the comments in `service/wire.rs`). Only the relay
  layer (`pulsar-proto`, bincode, positional) needs a protocol-version bump for changes.
  `StreamReq` already carries `width/height/fps/bitrate_kbps/quality/codec` — all
  live-changeable. `DataMsg::Stats(String)` exists as a free-form feedback channel.

## Design

### Principles

1. **One controller, in `pulsar-core`**, called by desktop and mobile. The two copies die.
2. **Transport-agnostic.** Inputs are measurements. The transport only contributes a hard
   ceiling (relay `rate_cap_kbps`) and a label for logs.
3. **Cheapest lever first:** bitrate (live where the encoder can) → fps → resolution
   (costs an encoder restart) → codec (last resort).
4. **Never freeze.** A loss must become a short softening: refresh-based recovery on the
   encoder, a keyframe request that never restarts capture, a receiver that never shows
   a frame with missing references.
5. **Startup fast, steady-state calm, emergencies immediate.** Probe up quickly in the
   first ~10 s (the maintainer accepts a short settle), then change slowly with
   hysteresis; halve at once on severe loss.

### Control loop

Tick every 500 ms; decide on 2 s windows.

**Signals** (per window): loss % (seq gaps); RTT and RTT trend/jitter (already
measured); one-way delay gradient from RTP timestamps vs arrival time (the GCC idea:
a rising gradient = a queue is building = back off *before* loss); received rate vs
requested rate; NACK success ratio (did retransmits arrive inside the repair window?);
decoder health from the renderer's stdout (frames dropped / corrupt); relay cap.

**Estimator:** `target_kbps = min(delay_based, loss_based, relay_cap, user_cap)`.
Delay-based backs off on a rising gradient; loss-based steps 0.7× on > 3 %, halves on
> 15 %, and only creeps up after a long clean stretch — and the creep must **stop
re-probing a punished ceiling**: after a probe causes loss, lower the ceiling 15 % and
double the wait before the next probe.

**Decision → operating point.** A ladder per codec, e.g. for H.264/HEVC:

| point | min kbit/s |
| --- | --- |
| 1080p60 | 8000 |
| 1080p30 | 5000 |
| 720p60 | 4000 |
| 720p30 | 2500 |
| 540p30 | 1200 |
| 360p30 | 600 |

Pick the highest point whose `min ≤ target × 0.85`. Go **down immediately** when
`target < point.min`; go **up** only after 20 s clean **and** `target ≥ next.min × 1.2`.
Resolution changes are debounced (they restart the encoder). The floor becomes
360p30 @ ~400 kbit/s — the old 2000 kbit/s floor goes.

**Actuation:** bitrate via live reconfigure where available (NVENC today; add AMF /
MediaFoundation / MediaCodec / x264 where the API allows), else via `StreamReq`;
fps/resolution via `StreamReq`. The client also sends a periodic `DataMsg::Stats` JSON
(loss, rtt, target, point) so a host-side controller (mobile host today, others later)
can act on the same numbers.

### Resilience — so "minimum" still looks clean

- **Intra-refresh** on every encoder that has it (NVENC `intraRefresh`, AMF, x264
  `intra-refresh=1`, MediaCodec `KEY_INTRA_REFRESH_PERIOD`, check rkmpp). Fallback where
  absent: GOP ≤ 0.5 s whenever measured loss > 0.5 %. Loss then heals in a fraction of a
  second with no bitrate spike.
- **Keyframe request is cheap everywhere:** force an IDR live; never restart capture.
  The ffmpeg CLI cannot be told to emit an IDR mid-stream, so on that path intra-refresh
  *is* the recovery (or move that host path to in-process libav — a later decision).
- **FEC:** XOR parity every N media packets, N adapted to measured loss, carried as a
  new `DataMsg`/media variant (session level → additive). Fills holes with **zero
  round-trips**, which is the only thing that works when RTT makes NACK too slow.
- **LTR** (long-term reference) where supported (NVENC, MediaCodec): on loss, encode
  the next frame against an acknowledged good reference instead of a full keyframe.
- **Receiver:** `max_delay` derived from measured RTT + jitter (capped), not fixed
  100 ms; the decoder keeps the last good frame and **never renders a frame whose
  references are missing** — skip until the refresh completes.

### Transport specifics

- **Relay:** `rate_cap_kbps` is a hard ceiling; expect higher RTT → size the repair
  window from RTT. **P2P / direct:** same loop. **LAN fast path:** may start higher.
- **Startup:** start at `min(base, 60 % of the last known-good rate for this peer)`,
  probe up aggressively for the first 5–10 s, then apply the steady-state rules.
  Remember the last good point per peer id.

## Phases — each one shippable and tested on its own

### Phase 0 — stop mid-stream freezes (desktop + core; the immediate fix)

| # | Task | Where | Done when |
| --- | --- | --- | --- |
| 0.1 | Feed RTT/jitter into the down-decision (mirror mobile's RTT-excess idea, then unify) | `hold.rs` | on `tc netem delay 120ms`, a rising RTT backs off before loss appears |
| 0.2 | Kill the sawtooth: punished ceiling ×0.85 + doubled wait; probe only ×1.1 after ≥ 60 s clean | `hold.rs` | no periodic loss storm over 10 min at a fixed 2 Mbit cap |
| 0.3 | Intra-refresh / short GOP on loss: host switches to refresh mode when the client reports loss > 0.5 % (`Stats`) | `pipeline/command.rs`, `pulsar-capture`, host `on_stats` | with 3 % loss the picture softens; no freeze > 300 ms |
| 0.4 | Cheap keyframe on the ffmpeg path: refresh instead of capture restart | `host.rs` on_nack | a keyframe request causes no capture gap |
| 0.5 | Receiver: `max_delay` from RTT; never show frames with missing refs | `pulsar-render/src/video.rs` (+ decode) | no `Could not find ref with POC` smear on screen |

Acceptance for the phase: 1080p HEVC **and** H.264, `netem loss 3% delay 120ms`,
5 minutes: no freeze > 300 ms; latency HUD stays sane; picture visibly softer, never stuck.

### Implementation status

**2026-09-03, second pass (after the maintainer's decisions on the open questions).** All
of it is in the three repos; the rate/recovery/FEC logic is unit- and simulation-tested,
the host-side engines compile and the libav engine has a headless end-to-end test — the
real-session matrix is the maintainer's.

**Maintainer's decisions (supersede the ones below in the first-pass table):**

1. **ffmpeg-path hosts → the ffmpeg libraries in-process** (`pulsar-desktop/src-tauri/src/host/libav.rs`,
   Linux): x11grab → libavfilter scale/format → libavcodec (libx264/x265/SVT-AV1, NVENC) →
   libavformat RTP, on a thread inside the app. Bitrate changes live (`bit_rate` reconfig in
   libx264/NVENC), a keyframe request sends the next frame as a forced IDR, the recovery
   modes force a keyframe every 0.5 s. Falls back to the CLI when the engine cannot start
   (device/encoder/output), or for VA-API/Vulkan/HDR/4:4:4. `PULSAR_LIBAV_HOST=0` disables it.
   Windows/macOS keep the CLI until the ffmpeg *libraries* are bundled for them (fetch script
   + release workflow work) — the engine itself is portable.
2. **FEC = Reed-Solomon per video frame** (`media::TAG_FEC_RS`, `RsFecEncoder`/`RsFecDecoder`,
   `reed-solomon-erasure`): `m = ceil(k × ratio)` parity packets per frame rebuild ANY `≤ m`
   lost packets of that frame — the Sunshine/Moonlight/Parsec model. The ratio follows the
   client's reported loss (`fec_policy::parity_ratio`: ≈2.5× loss + 5 %, 10–30 %, off after
   a clean stretch). Old XOR-only clients (v0.11.0) still get XOR groups; both apps' hosts
   and clients speak RS (`StreamReq::fec_rs`).
3. **Resolution and fps never change automatically.** They are fixed at session start
   (settings / what the display takes); the controller adapts the **bitrate** within them
   (plus FEC / recovery). `adapt::Config::ladder` is `false` in both apps; the ladder code
   stays available for a future opt-in.

| Area | What landed (second pass) | Where |
| --- | --- | --- |
| Wayland host | The portal → PipeWire → GStreamer pipeline runs **in-process** (`gstreamer-rs`): `set_bitrate` (x264enc/vaapi/nv `bitrate`, mpp `bps`), `request_keyframe` (force-key-unit event), `set_short_gop` (key unit every 500 ms). Bitrate and recovery re-requests are applied live; keyframe requests too. No more `gst-launch`, no portal restart. Build needs `libgstreamer1.0-dev` + `plugins-base` (CI updated) | core `capture.rs`, `pipeline/gst.rs` (`name=venc`), desktop `host/handlers.rs`, `host.rs` |
| NVENC native (Windows) | `CaptureConfig::intra_refresh` → periodic intra refresh (`enableIntraRefresh` + infinite GOP, one wave per second) when the client asked for it; the on-demand IDR stays. **Not compiled or run here** (Windows-only crate). LTR still deferred | `pulsar-capture` `lib.rs`, `encode.rs`, `encode/{nvenc,new}.rs`; `handlers.rs` |
| Mobile host | Reed-Solomon / XOR parity behind the video (ratio from client stats), `fec` / `fec_rs` features; `loss_recovery` plumbed to MediaCodec at encoder creation (`KEY_I_FRAME_INTERVAL` 0.5 s / `KEY_INTRA_REFRESH_PERIOD`) — a running encoder keeps its GOP (IDR-on-request covers it). Kotlin not built here | mobile `host.rs`, plugin `mobile.rs`/`desktop.rs`, `PulsarVideoPlugin.kt`, `HostEncoder.kt` |
| Mobile client | RS parity decode, `fec_rs`, per-peer last-good memory (`adapt-memory.json`, 60 % start) | mobile `client.rs`, `adapt_memory.rs` |
| Desktop | RS decode + XOR fallback, `fec_rs`; host sizes RS/XOR parity from stats; libav engine + live fast path + teardown + keyframe; `intra_refresh` to the native path | `play/hold.rs`, `play.rs`, `host.rs`, `host/handlers.rs`, `host/libav.rs` |

Still manual / not verifiable here: Windows (NVENC intra refresh, the reorder buffer),
macOS, the Android APK (Rust compiled + tested, Kotlin edited blind), and every real
session. The libav engine is exercised headless (`cargo test -p pulsar-tauri --lib libav`:
testsrc → libx264 → RTP, live bitrate, forced IDR, short-GOP cadence).

#### First pass (2026-09-03, earlier the same day)


All four phases were implemented on 2026-09-03 in the local checkouts (`pulsar-core`,
`pulsar-desktop`, `pulsar-mobile`), unit- and simulation-tested, **not pushed** (rule: no
behaviour change before the maintainer tested it). Push order once approved: core → `cargo
update -p pulsar-core` in desktop and mobile (delete each app's local `.cargo/config.toml`
path patch + `git checkout Cargo.lock` first) → desktop → mobile.

| Phase | What landed | Where |
| --- | --- | --- |
| **0** stop freezes | 500 ms pings, 100 ms NACK sweep + one re-NACK, repair window `1.5×RTT+100` (100–500 ms) mirrored live to the renderer's RTP `max_delay` (stdin `maxdelay`); renderer **holds** the last good frame ≤ 300 ms after an unrepaired loss (stdin `hold`, ends early on a keyframe; reported as `vidsink-hold` → `renderer loss hold` in the log); Windows/macOS depacketizer got a **reorder buffer** (retransmits used to be dropped as stale); `StreamReq.loss_recovery` (`short_gop` / `intra_refresh`) → `encode_command` emits `-intra-refresh 1` (x264, NVENC), `-int_ref_*` (QSV), `-intra_refresh_mb` (AMF H.264), else GOP ≤ 0.5 s; gst x11 `x264enc intra-refresh=true`; ffmpeg-path keyframe request no longer restarts capture once recovery is on; client→host `ClientStats` JSON in `DataMsg::Stats` (`on_stats` handler) | core `service/{wire,host,media}.rs`, `pipeline/*`; desktop `play/hold.rs`, `host.rs`, `host/handlers.rs`, `render_stats.rs`; `pulsar-render/src/{video,linux,desktop}.rs`, `win/mod.rs`, `stream/rtp.rs`; mobile `rtp.rs` (reorder window) |
| **1** ladder, one controller | **`pulsar_core::adapt`**: `Controller` (pure, 2 s windows) → target wire rate, operating point from a per-codec **ladder** (`adapt::ladder`, native-bounded, HEVC/AV1 ×0.7/×0.65), encoder bitrate net of FEC, recovery flip. Rate rules: delay first (Overuse ×0.85, RTT excess ≥35 ms ×0.7 / ≥90 ms halve, no climb while queued or draining), raw loss >15 % halve, sustained loss >3 % ×0.7 only with a queued link — with a flat link a single probe-down decides whether the loss follows the rate, otherwise it is learned as the path's **noise floor** (also: steady mild loss on a flat link is learned after 10 s); punished ceiling ×0.85, probes past it after 60 s clean, failed probes double the wait (≤16 min), surviving probes halve it; startup ×1.5 per clean window for 5 windows; ladder down at once when `target < point.min` (debounced 2 windows unless severe), up after 20 s clean at the point to the best rung under `target/1.2`; floor 300 kbit/s (360p30). **Desktop and mobile both call it** — `play/abr.rs` and mobile's `abr_decide`/panic reflex are gone. **Per-peer memory**: desktop remembers the last rate that stayed clean 30 s (`adapt-memory.json`) and starts the next session at 60 % of it. **Manual pins** (decision 3): an explicit session-menu resolution or fps pins the ladder (bitrate still adapts), "Otomatik" (0) hands it back; a manual bitrate pins the rate as before | core `src/adapt/{controller,ladder,delay,fec_policy}.rs`; desktop `play.rs`, `play/hold.rs`, `adapt_memory.rs`; mobile `client.rs` |
| **2** resilience | **2.1 FEC**: XOR parity over the media-over-session video flow (`media::TAG_FEC`, `FecEncoder`/`FecDecoder`, `parity_frame`/`recover`), one parity per `n` packets; the **host sizes `n` from the client's reported loss** (`adapt::fec_policy`: ≈2× the loss, **≤ 20 % overhead** (decision 2), covers keyframes too, off after 5 clean windows), only for clients that asked (`StreamReq.fec`); the client rebuilds a group's single missing packet with zero round-trips and deducts the parity share from the encoder rate. **2.3 live bitrate**: NVENC native and the Android MediaCodec host already reconfigure live; the ffmpeg/gst CLI paths cannot (decision 1: no in-process libav rewrite in this pass — they restart on a step, which the hysteresis keeps rare). **2.4** host-side `Stats` consumption = the FEC sizing + logging. **2.2 LTR deferred** (Windows-only NVENC code that cannot be built/tested here; MediaCodec has no public LTR API) | core `service/media.rs`, `adapt/fec_policy.rs`; desktop `host.rs` (`on_stats`), `host/handlers.rs` (forwarder), `play/hold.rs`; mobile `client.rs` |
| **3** delay gradient | `adapt::delay::Trendline` — WebRTC-style trendline estimator + over-use detector on per-frame RTP-timestamp vs arrival deltas (both apps feed it from their media path); `Overuse` cuts ×0.85 in the window and, via `Controller::poll_fast`, **between windows** after 500 ms of sustained overuse (≤1 cut / 1.5 s); `Underuse` (draining) blocks cuts and climbs | core `adapt/delay.rs`; desktop `hold.rs` (repair tick); mobile `client.rs` |
| **4** validation | **Simulated matrix** `pulsar-core/tests/adapt_scenarios.rs`: 8 netem-like profiles (20/5/2/1 Mbit × 2–250 ms × 0/1/3/10 % loss) × H.264/HEVC against a path model (capacity, bounded queue, random loss, FEC repair) — asserts settle-under-capacity, clean rung for the budget, ≤2 point changes after settling, recovery flip within 2 windows of loss, FEC ≤ 20 %; plus 20 Mbit→1080p60 ≤ 30 s, 2 Mbit→≤720p ≤ 10 s, delay-cut-before-loss. **Real-session half**: `pulsar-desktop/scripts/netem.sh` (tc, both directions) + `scripts/validate-log.mjs` (parses the daily log: holds > 300 ms, stalls, late down-steps/point changes, recovery flip latency, summaries; exit 1 on failure). The hardware matrix (AMF/NVENC/MediaFoundation × Windows/Pi decoders) stays manual — run the netem profiles and the script on each | core `tests/adapt_scenarios.rs`; desktop `scripts/` |

Decisions taken for the open questions (the maintainer may overrule):

1. **ffmpeg-path hosts** keep the CLI: intra-refresh/short-GOP recovery + FEC + NACK; no
   in-process libav rewrite (a separate, large project — the CLI paths only restart on a
   rate/point step, which the hysteresis makes rare).
2. **FEC**: ≤ 20 % overhead, sized ≈ 2× the measured loss, parity over every packet
   (keyframes included), off on a clean path.
3. **Manual pin**: the existing session-menu pickers pin a dimension; "Otomatik" unpins.
   The host's encode label (`hostenc …`) already shows the live point, so no new HUD strings.

Known limits of this pass:

- **Wayland hosts absorb** recovery/point changes like bitrate (a gst restart there is the
  black-video hazard in `handlers.rs`): no loss recovery or resolution ladder on Wayland;
  bitrate stays as before (also absorbed).
- **NVENC native** (`pulsar-capture`) is untouched: IDR-on-request works; intra-refresh/LTR
  need a Windows build + hardware (`TODO(A8)`).
- **Mobile host** keeps its 5 s GOP + `requestSyncFrame` on demand; `loss_recovery` is not
  plumbed into MediaCodec (a GOP change would need the encoder restart the plugin avoids).
- **Mobile client** asks for the short GOP (its depacketizer waits for an IDR); it does use
  FEC and the delay estimator; its ladder's top rung is the phone's requested size.

Test procedure (maintainer): `cargo build -p pulsar-render` first (`tauri dev` never rebuilds
the sidecar); `sudo pulsar-desktop/scripts/netem.sh up --loss 3% --delay 60ms` (both
directions → RTT ≈ 120 ms; `--rate 2mbit` for the cap check; `-i lo` for a self-connect on one
machine); run 5–10 minutes; then `bun scripts/validate-log.mjs <daily log>` (Settings →
General → Open log folder). Log lines to read: client `adaptive controller seeded`, `abr
step` / `abr fast step` / `abr window` (debug), `renderer loss hold`; host `client stats`,
`fec parity group size`. Env pins: `PULSAR_MAXDELAY` (µs) disables the live reorder wait.

### Phase 1 — the operating-point ladder (the heart of "cam gibi minimum")

| # | Task |
| --- | --- |
| 1.1 | Extract the controller into `pulsar-core::adapt` with a **pure, unit-tested** decision function: `(signals, state) → (operating point, actions)`. No I/O inside. |
| 1.2 | Ladder + hysteresis + debounce; per-codec tables; floor removed. |
| 1.3 | Desktop and mobile call the shared controller; delete both copies. Per-peer last-good memory. |

Acceptance: forced 2 Mbit → 720p30/540p30, clean, within 10 s; forced 20 Mbit →
1080p60 within 30 s; no oscillation over 10 min; identical behaviour desktop vs mobile.

### Phase 2 — resilience

2.1 FEC parity (adaptive N) · 2.2 LTR where supported · 2.3 live bitrate reconfigure
for AMF / MediaFoundation / MediaCodec / x264 · 2.4 host-side consumption of `Stats`.

### Phase 3 — delay-gradient estimator (GCC-style) replacing the loss-first heuristics.

### Phase 4 — validation matrix + harness

encoders (NVENC, AMF, MediaFoundation, MediaCodec, x264, rkmpp) × decoders × transports
(LAN direct, P2P punched, relay) × `tc netem` profiles (loss 1/3/10 %, delay 20/120/250 ms,
rate 1/2/5/20 Mbit). Scripted sessions on Linux, assertions on the client log
(freeze gaps, POC errors, "too late" counts, chosen operating point). Runs in CI where
the host side can be Linux-only; the Windows/AMF matrix stays manual.

## Constraints for whoever implements this

- **`pulsar-core` is a git dependency** of both apps: push core first, then
  `cargo update -p pulsar-core` in `pulsar-desktop` and `pulsar-mobile` and push those.
- **Session messages are JSON** (additive, `#[serde(default)]`); **relay messages are
  bincode** (positional — any change = `PROTOCOL_VERSION` bump in `pulsar-proto`, then
  relay, core, apps, and a relay redeploy). Keep adaptation at the session level.
- Tests must stay green: `cargo test` (core e2e), `bun run test:unit` + `cargo check -p
  pulsar-tauri` (desktop), `cargo test --workspace` (mobile). Add unit tests for the
  pure decision function — that is where the behaviour lives.
- UI copy is Turkish; new HUD strings go through i18n (4 languages).
- **Do not push a behaviour change before the maintainer has tested it** on a real
  session. Docs and tests may be pushed.
- The maintainer's `/home` is nearly full: delete `target/*/incremental` before big
  builds; do not create new target dirs.
- Read `pulsar-desktop/CLAUDE.md` ("Capability probe", "Client video") and the memory
  notes on the render sidecar before touching encoder/decoder selection.

## Open decisions (ask the maintainer before spending time)

1. ffmpeg-path hosts: intra-refresh-only recovery, or move to in-process libav?
2. FEC overhead ceiling (e.g. ≤ 20 % extra at 10 % loss) and whether to FEC keyframes only.
3. Should the user be able to pin an operating point (manual override), and how is it shown?
