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
  floor escape — `Sample::recv_bytes`/`window_ms` > 0 enables the goodput paths; both apps
  pass them since 2026-11-15).
  Client plumbing shared by BOTH apps (extracted 2026-11-15 from desktop `play/hold.rs` /
  `play.rs` / `adapt_memory.rs`, desktop behaviour unchanged): `repair` = the video repair loop
  (`RepairTracker`: seq gaps → reorder wait / FEC wait → NACK, one re-NACK ~1 RTT later,
  write-off after the RTT-derived window; window counters incl. goodput; FEC share learned per
  window and released when parity stops; `RttWindow`, `KeyframePacer` (`with_max`: a client
  that freezes until the keyframe caps the gap at `KF_GAP_MAX_FROZEN`), `FrameClock`,
  `decoder_overloaded` / `decoder_sheds`); `pong` = the uneven Ping cadence + `PongMatcher`; `caps` = `auto_cap_kbps` (15-150 Mbit, mobile
  ≤ 50 Mbit) + `path_cap_kbps` (relay cap while relayed); `memory` = per-peer × `PathClass`
  (lan / wan / relay) last-clean-rate file. With the ladder off (mobile) the decoder limit is
  `Controller::client_decoder_sheds` (one fps rung down at the same size, the rung it left
  retried after `POINT_RETRY_WINDOWS`, doubling); `client_decode_bound` (decode time) stays the
  ladder-on path. `RepairTracker::reset_stream` = a client re-request rebuilt the encoder.
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
- `input/keytables.rs` + `input/mouse_button.rs` — pure, tested everywhere:
  `CLIENT_KEYS` (every evdev code a client sends; each host table is tested against it), the
  macOS host table `mac_vk` (ISO swap), the Linux host's char → key table builder
  (`build_char_table`, fed from libxkbcommon by `host_keymap_linux.rs`: Wayland `wl_keyboard`
  keymap / X11 `_XKB_RULES_NAMES`, read on a process-wide worker thread, never on the input
  path; main-block keys win over extended ones; a Caps table when KEY_CAPSLOCK locks Caps;
  dead-key-only chars as dead key + Space; misses go to 32 LRU X11 scratch keycodes or the
  GTK/IBus Ctrl+Shift+U entry), and the wire mouse button ids (3 = back, 4 = forward,
  `FEAT_MOUSE_X`; hosts ignore unknown ids and drop side-button ups they never pressed). Linux code here is type-checked from Windows with
  `cargo check --target x86_64-unknown-linux-gnu` and `SYSTEM_DEPS_<LIB>_NO_PKG_CONFIG=1` +
  `SYSTEM_DEPS_<LIB>_LIB=<name>` for the glib/gio/gobject/gstreamer(-base/-video) sys crates.
- `input/` — controller types/normalization + virtual pads: uinput (Linux),
  ViGEmBus X360+DS4 (`vigem.rs`), Win32 `SendInput`, macOS CGEvent, and the
  per-window co-op injector `WindowInput` (same API on each OS: `window_win.rs`
  PostMessage / `window_x11.rs` X11 SendEvent+XTest / `window_mac.rs`
  CGEventPostToPid; pure helpers + tests in `window_common.rs`; native Wayland is
  impossible; Linux/macOS untested on hardware; `is_alive`/`in_front`/size/X11 child
  under the pointer are cached 200 ms — the host asks per input event),
  hi-res scroll (`wheel.rs`: wire `Scroll` = browser px, 100 per notch, fractions kept →
  Windows desktop session `mouseData` in 1/120 notch, but WHOLE notches (×120) in a game
  session / while the client pointer is relative (`DesktopInput::set_whole_notch_wheel`,
  set by the host app) and always on the per-window injector — games do `delta / 120` as
  an integer; Linux `REL_WHEEL_HI_RES` + legacy notch at the halfway point like the
  kernel's hid scroll counter; macOS one scale: LINE events, finer deltas with fractional
  lines in the fixed-point fields (+ points at 10/line, unmeasured on hardware); X11
  per-window stays whole notches — core events have no finer unit), client motion
  batching (`coalesce.rs`: token bucket per motion kind, 2 deep, one per 0.8 ms — a 1 kHz
  mouse with jitter is never held; `push_backlog` merges a channel backlog but sends it at
  once; edges flush first; used by the desktop session loop),
  DS4/DS5 touchpad-as-mouse (`touchpad_mouse.rs` = the shared,
  unit-tested gesture machine — pointer / two-finger scroll / click / two-finger right
  click — used by the Linux evdev reader `touchpad_linux.rs` and by the desktop app's
  SDL3-based Windows/macOS reader; untested on hardware; `pad_route.rs` = which client
  session a local pad drives — tabs / split panes / locks / press-to-join — shared by the
  desktop app's gamepad forward gate and its touchpad readers). `input/pen_touch/` = pen + multi-touch:
  wire state types (`PenState`, `TouchFrame` — `InputEvent::Pen` / `Touch`, host features
  `FEAT_PEN` / `FEAT_TOUCH`), the host `Driver` (sanitize, slot tracking, 1 s silence lift,
  keep-alive refresh) on a worker thread (`PenTouch`) feeding a platform `Sink` — Windows
  synthetic pointer devices (`CreateSyntheticPointerDevice`, resolved at run time), Linux uinput
  tablet + multitouch type B, macOS CGEvent tablet events for the pen and touch-as-mouse
  (`mouse::TouchMouse`; macOS has no public touch injection) — plus the client gate
  `client::ClientPenTouch` (feature check, mouse fallback for old hosts, 250 ms keep-alive,
  30 s stale lift, 2.5 s for hover only). The Windows sink injects through a `PointerBackend`
  (`LocalDevices` in-process, or the desktop app's SYSTEM helper via plain-data
  `SynthContact`s). `tablet` = client-side native tablet attributes for webviews that report a
  pen as a mouse (macOS listen-only CGEventTap, Linux evdev reader; `TabletAugment` turns the
  mouse into `Pen`). `avf_screens` maps an avfoundation capture device to its CG display (macOS
  host geometry, `DesktopInput::set_display`). Linux skips the streamed-monitor region for the
  direct devices on GNOME / KDE Wayland (`region_applies`, unverified assumption).
  `DesktopInput::pen` / `touch` on every platform (no-ops in the stub).
  Untested on pen / touch hardware. **Controller *reading*
  moved to SDL3 in the desktop app — gilrs is gone** (ignore stale doc comments
  saying otherwise). **macOS virtual pad** = `mac_hid.rs`: `IOHIDUserDeviceCreateWithProperties`
  (symbols via `dlsym`, macOS 10.15+) impersonating an Xbox Wireless (BT, 045E:0B13) or DS4 (USB,
  054C:09CC) pad; descriptors / reports / rumble decoding are pure in `virtual_hid.rs` (tested on
  every host). Needs the restricted entitlement `com.apple.developer.hid.virtual.device`;
  without it creation returns NULL → `PadFailure::MacEntitlement` (checked via
  `SecTaskCopyValueForEntitlement`) → `RecordingPad`. `create_virtual_pad_checked` returns the
  `PadFailure` for every OS (`vigem_missing`, `uinput_denied`, …); the app relays it as
  `DataMsg::PadUnavailable { slot, reason }`. Untested on macOS hardware.
  **Input permission** (`input/permission.rs`): `preflight()` = can this host inject desktop
  input at all (Linux `/dev/uinput` writable, macOS `AXIsProcessTrusted`; never prompts),
  `foreground_window_blocked()` (Windows, keys) / `pointer_target_blocked()` (Windows,
  clicks / scroll / pen / touch: the top-level window under the cursor) = that window belongs
  to a higher-integrity process (token integrity RID; when the token is closed, "query yes /
  `PROCESS_VM_READ` no" = no-read-up) so UIPI drops the input; the secure desktop (UAC
  prompt, lock screen: input desktop not `Default`) is reported as `SECURE_DESKTOP` (-1).
  `InputFailure` wire tokens (`uinput_denied`, `mac_accessibility`, `win_elevated`,
  `win_secure_desktop`, `android_control_off`, `android_control_missing`, unknown → `unavailable`) travel as
  `DataMsg::InputUnavailable { reason }`.
  **Touchpad pinch injection** (`DesktopInput::pinch`, `InputEvent::Pinch`, advertised by the
  desktop host as `FEAT_PINCH`): Windows = two-finger touch injection (`windows.rs`); Linux =
  a lazily created virtual uinput TOUCHPAD ("Pulsar Virtual Touchpad", `uinput.rs`) whose two
  contacts a worker thread spreads/narrows at ~60 Hz so libinput reports a real pinch to the
  app under the pointer (held ≥ 260 ms so it never reads as a two-finger tap; fingers lift
  after 1.5 s of client silence); macOS = a trackpad magnify gesture posted with the window
  server's private gesture event layout (`kCGSEventGesture` 29, fields 110/113/132 — the same
  one Hammerspoon posts; `macos.rs`). Linux and macOS: **untested on hardware**.
  **Touchpad gestures** (`input/gesture.rs`, `InputEvent::Gesture { fingers: 3|4, kind:
  SwipeUp|SwipeDown|SwipeLeft|SwipeRight|Tap }`, advertised as `FEAT_GESTURE`; old hosts drop
  the unknown variant): the host does what ITS user configured for the gesture.
  `HostGestures::action` reads the settings on every gesture and returns a `GestureAction`
  (`Chord` of evdev keys / `Click` / `Nothing`) that the app injects with its ordinary key /
  button calls. Pure, unit-tested parsers per OS: `win` (HKCU, then HKLM, then twinui's
  defaults, of `…\PrecisionTouchPad`: `Three/FourFingerSlideEnabled` 0 nothing / 1 apps /
  2 desktops / 3 audio / 0xFFFF custom, `Three/FourFingerTapEnabled` 0 nothing / 1 search /
  2 notification center / 3 play-pause / 4 middle click / 0xFFFF custom; custom per-direction
  `ThreeFingerUp`… codes 0-18 and `CustomThreeFingerTap`… codes 0-7, `…KeyParams` = DWORD low 16
  bits modifiers (1 Ctrl, 2 Shift, 4 LWin, 8 RWin, 0x10 Alt) + high 16 bits VK — read from
  `SettingsHandlers_nt.dll` / `twinui.dll`, 2026-11-15), `mac` (trackpad prefs + Dock gesture
  switches → Spaces ^←/^→, Mission Control ^↑, App Exposé ^↓, page swipe ⌘[/⌘], Look up ⌃⌘D;
  `dock_swipe_fingers` for the macOS client), `linux` (GNOME / KDE defaults from
  `XDG_CURRENT_DESKTOP`, kwinrc overview + desktop count). A swipe down right after the swipe up
  that opened an overview sends the same toggle shortcut again (closes it). The Windows
  injector maps the media keys (113-115, 163-165) and browser back / forward (158 / 159) now.
  **Untested on hardware** (all three hosts).
  - `input/local_pointer_macos.rs` — macOS host: listen-only `CGEventTap` noticing the
    person at the host using their own mouse (desktop `host::takeover`); skips events tagged
    `INJECTED_TAG` and events from our own pid; `request_listen_access`,
    `pointer_on_main_display`. **Tag contract:** every event `input/macos.rs` posts carries
    `INJECTED_TAG` (`"PULS"`) in `kCGEventSourceUserData` — a new injection path must set it
    too or its events count as the local user. Untested on macOS hardware.
- `overlay_macos.rs` — macOS: `make_overlay_window` turns an NSWindow (the desktop's host-side
  remote-cursor ghost) into a system-wide click-through overlay (status level, all Spaces).
  Over another app's full-screen Space unverified (may need an NSPanel). Untested on macOS
  hardware.
- `media.rs` — capture→encode→transport→decode traits + RLE software baseline
  **for tests only**. Real encoders live in the apps (see below).
- `pipeline/` — **pure** ffmpeg/gst arg builders + `HwEncoder`/`VCodec`
  detect/resolve. No process spawning here (that's the Tauri layer).
  `StreamPlan.window` = one window instead of the screen: `WindowInput::X11 { id }` for
  x11grab (`-window_id`, scaled to the plan size; `fit_window_size` / `fit_within` pick it)
  — used by both the per-window "Pencere" pick and the Linux per-window co-op capture
  source — or `WindowInput::RawNv12` (macOS ScreenCaptureKit frames).
  SDR colour rule (2026-11-15): every encode command signals **BT.709 limited range**
  (`command.rs` `SDR_ENCODER_OPTS` + a `setparams` tag; gst `videoconvert` caps carry
  `colorimetry=bt709`); macOS ScreenCaptureKit is set to the BT.709 matrix from sRGB
  (`macos_capture::sck::set_sdr_colour`). Clients decode the stream's tag, BT.709 limited
  when absent (pulsar-desktop `pulsar-render/src/colour.rs`). Every RGB → YUV conversion
  the ffmpeg CLI runs names the matrix (`SDR_SCALE_OPTS`: x11grab / gdigrab `scale`, the
  ddagrab CPU bounces, `scale_cuda` and `vpp_qsv` options), so an FFmpeg older than 7.1
  (Rockchip PPA, system ffmpeg) cannot convert BT.601 under the BT.709 tag; only ddagrab →
  NVENC without the CUDA map hands NVENC the BGRA. SCK's CF strings go through `msg_send!` as
  `ffi::CFStringObj` pointers: objc2 checks argument encodings in debug builds and panics on
  `*const c_void` (`^v`) where a method takes `^{__CFString=}`.
- `x11win.rs` — X11 top-level window list (`_NET_CLIENT_LIST`) and launched-game →
  window lookup (`_NET_WM_PID` over the `/proc` process tree) for the co-op path; the Linux
  counterpart of `pulsar-capture`'s `window.rs`. Overlaps `window_capture/x11.rs` (the
  client's window picker list) — two listings of the same EWMH data, kept separate since
  the merge (`parity/all`); a later cleanup can fold one into the other. Empty on Wayland.
  Untested on Linux hardware.
- `capture.rs` — **Linux-only** (`cfg`): Wayland XDG ScreenCast portal →
  PipeWire → an **in-process GStreamer pipeline** (gstreamer-rs; live `set_bitrate`,
  `request_keyframe`, `set_short_gop`); `is_wayland()`. Build needs the GStreamer dev
  headers (`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`).
  Colour conversion (2026-11-15, untested on hardware): `videoconvert n-threads=0` everywhere;
  with a `vah26Xenc` fragment the in-process pipeline first tries `vapostproc` → VAMemory NV12
  (GPU, zero-copy where PipeWire negotiates DMA-BUF), verified by a frame reaching the encoder
  within 3 s, else the CPU convert (`gst::WaylandConvert`, `PULSAR_WAYLAND_CPU_CONVERT=1`).
  The GPU attempt goes first in each of the 3 start slots (a "not PLAYING" failure is usually
  the PipeWire node lagging the portal reply), with the CPU convert right after it in the same
  slot; a parse failure or no frame after PLAYING marks it broken for the process
  (`VA_CONVERT_BROKEN`). `run_pipeline` is cancel-safe (`NullOnDrop`: a start future dropped
  during the verify wait sets its pipeline to NULL).
  `start_with_frames(.., Some(factory))` (`capture/frames.rs`, 2026-11-14, untested on
  hardware): the pipeline ends in NV12 frames (`fakesink` handoff) for an app-supplied
  `FrameEncoder` (the desktop's NVENC SDK encoder — reference-frame invalidation via
  `WaylandCapture::request_rfi`), built on the first frame; on failure / no frame in 4 s the
  gst-encoder pipeline runs on the same portal session. If the in-process
  GStreamer lacks an element of the pipeline, the same description runs under the system
  `gst-launch-1.0` (fd inherited; no live controls, `is_in_process() == false`);
  `use_system_gst_env` strips an AppImage's bundled-GStreamer env (incl. `$APPDIR` entries of
  `LD_LIBRARY_PATH`/`PATH` — else the "system" tool is, or loads, the bundled one) from such children.
  `start(.., system_gst)` forces that child (caller validated the encoder only on the
  system gst). `self_check(fragment)` = headless in-process encode test (desktop `--selftest`).
  gst VA-API = two families, one wire id `vaapi`: `Vaapi` (gstreamer-vaapi, `vaapih264enc`)
  and `Va` (gst-plugins-bad `va`, `vah264enc`, what gst ≥1.26 distros ship).
  `start_with(.., Source::Window { max })` = a WINDOW cast (portal offers windows only, the
  host person picks; frames scaled into `max` via `gst::wayland_pipeline_sized`).
  `start_pipeline(desc, on_encode_ms, first_frame_within)` (`capture/live.rs`, 2026-11-15,
  untested on hardware): the same in-process runner for a complete description WITHOUT a
  portal — the desktop's X11 `ximagesrc ! … ! mpph26Xenc` pipeline on RK3588 hosts — returning
  a `WaylandCapture` (no session / PipeWire fd) with the same live controls, a std-thread
  bus/short-GOP watcher (no tokio runtime needed) and an encode meter (pad probes on `venc`:
  `frames_encoded()` + `pipeline::gst::EncodeTimer`, the real per-frame encode time). A live
  source never prerolls, so it returns on the encoder's FIRST frame (not "PLAYING") and fails
  with a `StartError`: `Unavailable` (init / missing element / no `venc` → run `gst-launch`),
  `Failed` (bus error / EOS first), `NoFirstFrame` (ran, encoded nothing — the RGA >4G BGRx
  case). Blocking: callers run it off their async loop. The vendor encoder then lives in the
  app process (a crash there is the app's). Not for `kmssrc` (needs the CAP_SYS_ADMIN child
  binary).
- `window_capture/` — host **per-window** capture off Windows (Windows = WGC in the desktop's
  `pulsar-capture`). `backend()` (X11 / Portal / ScreenCaptureKit / None), `list()`, `size()`,
  `find_for_launch(pid)` (largest window of the pid tree). X11 (`x11.rs`, pure-Rust `x11rb`,
  `_NET_CLIENT_LIST`): id = X window id, grabbed by `pipeline::WindowInput::X11`
  (`x11grab -window_id`) or `gst::x11_window_pipeline`. macOS (`mac.rs`, ScreenCaptureKit
  12.3+): id = `CGWindowID`; `serve_for(session, id, max, fps)` starts a desktop-independent
  window stream (its aspect fitted into `max`) and serves raw NV12 on loopback TCP for
  `WindowInput::RawNv12` (`-f rawvideo`), returning url + size from one query;
  `stop_for(session)` stops it. Wayland lists nothing (`WindowInfo::PORTAL_PICK`).
  Also: `rect(id)` (X11 root pixels / macOS global points, no SCK query), `pointer_region(id)`
  (Linux: the window as a 0..1 uinput region of the root), `raise(id)` (X11
  `_NET_ACTIVE_WINDOW`; macOS app activate + `AXRaise`), `fit(id, pane_w, pane_h)` /
  `restore(prev)` (X11 `_NET_MOVERESIZE_WINDOW` on the RandR monitor clipped to
  `_NET_WORKAREA`; macOS Accessibility `AXPosition`/`AXSize`). macOS
  `input::DesktopInput::set_window_rect` maps absolute moves into a window frame.
  ScreenCaptureKit is linked strongly by `objc2-screen-capture-kit`: the app binary weak-links
  it (desktop `src-tauri/build.rs`).
  Wire meaning of the id per OS: `service/wire.rs` `WindowInfo`. **Untested on Linux / macOS
  hardware** (2026-11-14, branch `parity/window-capture`).
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
- `call.rs` — voice calls over a session, shared by both apps: the phone-style state machine
  (`Call`: `action` / `signal` / `tick` / `gone` → `Step` = signal to send + phase change + why
  it ended), the loss rules (caller repeats `Ring` every 2 s; repeated `Ring` re-answered or
  re-declined; final signals sent 3×; both sides time a ring out) and the built-in ringtones
  (`tone_pcm`: synthesized s16le 48 kHz mono, no audio assets). Wire: `DataMsg::Call(CallSignal)`
  (appended; old peers ignore it → the call rings out); the voice is `DataMsg::Audio` both ways.
  Host side: `DataHandlers::on_call`. `CallSignal::Hello` on session start (`HELLO_AT` schedule,
  answered once) = "I take calls": apps offer calling only to such a peer (`Call::supported`).
- `audio/aec.rs` — echo canceller for calls (pure Rust, own radix-2 FFT): partitioned-block
  frequency-domain NLMS, 256-sample blocks × 40 partitions (~213 ms tail), two-path
  (background adapts, foreground copied when clearly better / background reset when it
  diverges = double-talk safe without a detector), residual suppression −12 dB while echo-only.
  `AecStream` takes any chunk size (+1 block latency). Synthetic tests: room echo ~59 dB, digital
  loopback ~61 dB, near talker kept during double-talk; ~1 % of a core in release, ~28 % debug.
- `audio/` — WASAPI loopback (+ per-process), Opus/RTP command builders,
  mute policy (`AudioSettings::policy` — game mode transmits but never
  force-mutes the captured endpoint), endpoint redirect sinks. macOS: `sck_loopback.rs`
  (`run_system_audio_capture`, the system mix via ScreenCaptureKit — the counterpart of the
  Windows process loopback in `Exclude` mode); `Config::audio_loopback()` = "system mix" on
  Windows and macOS when no device is named. The ffmpeg pulse input reads 10 ms fragments
  (`-fragment_size 1920 -fflags nobuffer`). Client side: `audio::AudioRepair` (`repair.rs`,
  used by desktop `native_audio` and the mobile feeder) puts audio RTP back in order, rebuilds
  losses from `TAG_AUDIO_FEC` parity and holds a hole for a caller-chosen time before it
  reports it lost, counted from the earliest arrival behind the hole (no packet waits longer
  than the hold; a second hole gets its own time once the first fills). Parity is taken only
  in audio shapes (k ≤ 4, m ≤ 2, shard ≤ `FEC_MAX_PACKET` + 2), so stray datagrams can't make
  the decoder store big blocks (2026-11-15, untested in a real session).
- `displays.rs` (+ `displays/{macos,linux}.rs`) — the host's monitors for the platforms the app
  can't enumerate itself: macOS (`CGGetActiveDisplayList` order = avfoundation `Capture screen N`,
  `NSScreen.localizedName` names, `CGDirectDisplayID` as `Monitor::id`) and Linux Wayland
  (`wl_output` + xdg-output). `watch()` = change events: CoreGraphics reconfiguration callback
  (macOS), Wayland `wl_output` / X11 RandR (Linux, x11rb). `index_of_rect` maps the portal's
  stream geometry to a list index; `pointer_in` = the host pointer inside a monitor (macOS).
  `watch()` never blocks and can be called again to restart a dropped Linux watcher. A monitor
  without a platform name has an empty `name` + `builtin` flag (the app localizes the label).
  `DataMsg::StreamedDisplay(idx)` (service/wire.rs) tells the client which monitor the Wayland
  portal pick really streams. Pure helpers unit-tested; untested on macOS / Wayland hardware.
- `discovery.rs` — LAN UDP multicast beacon (239.255.71.21), multi-NIC. The beacon name is a label the user chose or the
  hostname (never the OS user's real name), and apps keep the beacon silent (`set_paused`) while unattended access is on.
- `netpath.rs` — shared path facts: `classify` (Lan / Wan / Relay; overlay VPNs = Wan via
  interface name, the routed adapter's type and a 12 ms RTT fallback), the overlay /
  virtual interface name lists, and the UDP socket-buffer tuning + report (`SO_RCVBUFFORCE`
  on Linux, a WARN under 1 MiB, `--selftest` check, `UdpRcvbufErrors`).
- `service/` — the app protocol over the encrypted session: OTP auth (non-empty
  passwords via **SPAKE2 PAKE** in `service/pake.rs`, MAC'd over the channel binding —
  the password never travels; new clients refuse to send one to an old host
  → `ConnError::PeerOutdated`; hosts still accept legacy plaintext from old clients),
  keepalive `Ping`/`Pong`/`Bye` (no ping id — the desktop client's `PongMatcher` copes),
  media framing + NACK + **FEC parity** — Reed-Solomon (`TAG_FEC_RS`; `RsFecEncoder::new`
  groups up to 3 small frames within 25 ms into one block, `per_frame` = old behaviour,
  released decoders unchanged), XOR kept for v0.11.0 clients, audio parity (`TAG_AUDIO_FEC`
  0x07, `AudioFecEncoder`: RS(2+1) per 10 ms Opus / RS(4+2) per 5 ms L16, only to clients
  with `StreamReq::audio_fec`), `MediaSendStats` send counters
  (`media.rs`), and **`DataMsg` side channels** (clipboard/chat/file/mic-audio/
  avatar/fs-browse/client `Stats` JSON/…) in `wire.rs`.
  **`DataMsg` lives HERE, not in pulsar-proto** — proto only covers the
  relay/transport layer.

**Division of labor:** this repo has NO real video encoder/decoder. Windows
native capture+encode = `pulsar-desktop/crates/pulsar-capture` (DXGI→NVENC SDK);
rendering = `pulsar-desktop/crates/pulsar-render`; Android encode =
pulsar-mobile's Kotlin plugin. Here: traits, arg builders, and the Linux
Wayland capture.
