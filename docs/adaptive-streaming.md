# Adaptive streaming — design and plan

**Status:** approved design, not started. Next major task (see `AGENTS.md`).
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
