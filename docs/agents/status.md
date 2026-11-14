# Status — what an agent picking up the repo should do next

Part of the pulsar-core agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Pen + multi-touch — branch `parity/pen-touch`, LOCAL, untested on hardware

`input/pen_touch/` (see [modules.md](modules.md)): `InputEvent::Pen` / `InputEvent::Touch`,
features `pen` / `touch`, injection on Windows (synthetic pointer devices), Linux (uinput
tablet + MT type B) and macOS (CGEvent tablet; touch mapped to mouse + scroll — no public
touch injection exists). Old peers: an old host's `dec` drops the unknown variant, and new
clients only send them to hosts advertising the feature (others get mouse events). Unit
tests run on Windows + Linux; macOS is type-checked only. Never run with a real pen or
touchscreen on any platform; desktop counterpart on the same-named branch. Since the review:
Windows `PointerBackend` (SYSTEM-helper path), hover stale limit, client native tablet capture
(`pen_touch/tablet.rs`: macOS CGEventTap, Linux evdev — Linux code is type-checked only, its
tests are pure and run everywhere), macOS streamed-display geometry (`avf_screens`), Linux
Wayland region policy. Mobile's host
`match ev` has a wildcard, so it keeps compiling on the bump (it ignores pen / touch).
## macOS host virtual gamepad — written, untested on macOS hardware (2026-11-14)

`input/mac_hid.rs` + `input/virtual_hid.rs` (see [modules.md](modules.md)). To verify on a
Mac: build signed with the virtual-HID entitlement + provisioning profile (pulsar-desktop
`src-tauri/entitlements/virtual-hid.plist`, release.yml "macOS signing overlay"), host a game
session, check the pad appears in System Settings → Game Controllers / an SDL test app,
buttons/sticks/triggers/d-pad map right for both targets, and rumble reaches the client.
Unsigned build: expect `PadFailure::MacEntitlement` in the log and the client notice. Open
question: do GameController and SDL accept a virtual device's VID/PID as the real pad (the
DS4 calibration/serial feature reports are answered; motion stays zero).

## Adaptive streaming rework — shipped, awaiting the maintainer's real-session test

**Design, phased plan and implementation status: [`../adaptive-streaming.md`](../adaptive-streaming.md).**
Read it end to end before touching the streaming path. Summary:

- Mid-stream the picture must **never freeze**; the client measures continuously and
  adapts automatically, on every encoder/decoder/transport, to the minimum bandwidth
  the path allows with the cleanest possible picture at that budget.
- Before the rework: two divergent bitrate-only controllers (desktop `play/hold.rs`, mobile
  `client.rs`), no resolution/fps ladder, no intra-refresh/FEC/LTR, 2 s GOP in remote
  mode, NACK useless when RTT > 100 ms → each loss was a multi-second freeze.
- **Phases 0–4 shipped 2026-09-03** (core v0.6.0 → desktop v0.11.0 / mobile v0.12.0),
  then a second pass on the maintainer's decisions (core v0.7.0 → desktop v0.12.0 / mobile
  v0.13.0): in-process libav host (Linux), in-process GStreamer for Wayland, Reed-Solomon
  FEC, resolution fixed / bitrate-only adaptation, NVENC intra refresh (Windows, unbuilt
  here), mobile host FEC + recovery. The controller is `src/adapt/` (pure, unit + scenario
  tested: `cargo test --test adapt_scenarios`).
- **None of it has been tested on a real session yet.** The next step is the maintainer's
  test (procedure in the design doc's "Implementation status"), then fixes from it.
- **2026-11-09 poor-link pass — LOCAL, unpushed, untested on a real link** (design doc section
  "2026-11-09 poor-link pass"): controller cautious start/soft pins/goodput cut/floor escape,
  grouped delay estimator, FEC v2 policy + grouped RS blocks + send counters, path watchdog +
  NAT rebinding. Mobile `client.rs` builds `adapt::Sample` without `..Default::default()` →
  breaks on the next core bump; `mobile/src/host.rs` still uses v1 `parity_ratio`.
