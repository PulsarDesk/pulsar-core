# DualSense passthrough — design

Status (2026-11-11, branch `feat/dualsense`, local only): **implemented and verified in a
real session** — Windows host (x64, usbip-win2) ⇄ Windows ARM64 client with a Bluetooth
DualSense: adaptive triggers, lightbar, HD haptics and the pad speaker work; local bridge mode
(`pulsar --ds5-bridge`) verified too. Not done: pad mic (needs a host→client mic-open
message), USB-pad audio on the client, settings UI + auto-degrade, installer bundling,
Duo backend (HID only, written, untested on hardware). Linux host: virtual pad attach +
haptics verified on an Orange Pi (no game yet). Untested: Linux client (BT pad), macOS.
Protocol reference: see "Protocol notes" below and the sources it lists.

## Goal

A DualSense on the client behaves on the host like a DualSense plugged into the host by
cable. The game gets the real thing, so every feature it drives works with no extra tool on
either side: adaptive triggers, lightbar / player LEDs / mic LED, classic rumble, HD
(voice-coil) haptics, speaker + headset jack, microphone, gyro, accelerometer, touchpad.
This works whether the pad is on the client by USB or by Bluetooth. Example: a Pi client
with a BT DualSense plays a Windows host like a PS5.

Second product from the same code: **local bridge mode** on one machine. A BT DualSense is
shown to local games as a wired one (the same job ZerkSense5 / DSX / DS5Dongle do), with no
streaming session involved.

## Scope v1

| Role | Platforms | Notes |
| --- | --- | --- |
| Host (virtual wired DualSense) | Windows, Linux | Windows: usbip-win2 (x64 + ARM64, signed). Linux: in-kernel `vhci-hcd`. macOS host: out (virtual USB/HID needs Apple entitlements; the macOS pad is a stub today). |
| Client (drives the physical pad) | Windows, Linux, macOS | BT and USB. |
| Local bridge | Windows, Linux | Same server as the host, fed by a local pad. |
| Deferred | iOS client (no iOS app exists yet — pulsar-mobile is Android-only), Android client (no app access to raw BT HID), DualSense Edge extras (presented as a plain DualSense). |

Every feature can be toggled on the client (Settings → Kontroller), and an "Otomatik" mode
degrades on a weak BT link (see "BT budget").

## Why the OS doesn't do this already

- Windows: no DualSense driver at all; the pad is generic HID plus, on USB only, a 4-channel
  UAC1 audio device (channels 1/2 = speaker, 3/4 = haptics). Games write HID output reports
  and PCM themselves. Over BT there is no audio device, so games have nowhere to send
  haptics or speaker audio.
- Linux: `hid-playstation` covers input/gyro/touchpad/LEDs/classic rumble on USB + BT;
  triggers only via hidraw from userspace; USB haptics via `snd-usb-audio` 4ch; no BT
  haptics.
- macOS: GameController covers input + `GCDualSenseAdaptiveTrigger`; haptics only through
  Core Haptics (not PCM); no BT haptics.
- BT haptics/audio = Sony-specific HID output reports (0x32…), reverse-engineered by
  SAxense (egormanga) and DS5Dongle (awalol), both MPL-2.0; ZerkSense5 (MIT) does the full
  Windows bridge. Verified on the maintainer's Win11 ARM64 PC on 2026-11-11 (ZerkSense5
  ARM64 build + usbip-win2 0.9.8.1 ARM64: haptics felt over BT).

## Architecture

```
CLIENT                                   HOST
physical DS5 (BT or USB)                 game
   ▲   │ input report                       ▲  │ HID OUT / iso audio OUT
   │   ▼                                    │  ▼
ds5::Pad (hidapi owner)                  Windows/Linux USB stack
   ▲   │                                    ▲  │  (usbip-win2 / vhci-hcd)
   │   │  InputEvent::Ds5Input (raw)        │  ▼
   │   └──────────────────────────────►  ds5::VirtualDs5 (USB/IP server, 127.0.0.1)
   │  DataMsg::Ds5Output (raw 0x02)         │
   │  RTP PT 99 haptics (3 kHz s8 st.)      │
   │  RTP PT 97 speaker (Opus) [existing]   │
   └────────────────────────────────────────┘
   mic: client capture → Opus → host → virtual mic iso IN
```

### Host: `VirtualDs5` (pulsar-core `input/ds5/virtual_*`)

- A minimal **USB/IP 1.1.1 server** on 127.0.0.1 (configurable port; usbipd-win commonly
  owns 3240, so default to an ephemeral port and pass `--tcp-port`). It exposes one
  emulated DualSense: device/config descriptors, the HID interface (interrupt IN/OUT), UAC1
  control + isochronous speaker OUT (48 kHz, 4ch, s16) + mic IN.
- Attach: Windows `usbip.exe attach -r 127.0.0.1 -b <busid>` (usbip-win2). Linux writes
  `vhci-hcd`'s sysfs `attach` file in-process, with no CLI (`input/ds5/vhci.rs`, see "Linux").
- Feature report 0x09 (pairing info) returns a **locally administered** MAC so Steam /
  GameInput don't mistake the virtual pad for the cabled twin of a BT pad and drop the BT
  link (Windows error 1167; documented by ds5-bt-haptics/ZerkSense5).
- Implements `VirtualGamepad` (so the slot machinery, hotplug and teardown are unchanged)
  plus a new `Ds5Sink` side: `apply_raw(input_report)`, plus streams of host→client events
  (`OutputReport`, `HapticsPcm`, `SpeakerPcm`) and client→host mic PCM.
- **We are the audio device**, so no WASAPI capture is needed: iso OUT packets arrive in
  our server as 4ch 48 kHz PCM. Channels 1/2 → speaker path (Opus, existing encoder);
  channels 3/4 → haptics path.
- Default-device guard: when the virtual pad appears, Windows may make its speaker the
  default output. The host records the default endpoint before attach and restores it if
  it changed, so desktop/game audio keeps going to the host's normal sink (and so the
  existing loopback stream doesn't capture silence).
- Selected when the client sends `kind = Ds5` with `target = Ds5` (new `EmulationTarget::Ds5`)
  or `Auto` **and** the host has the backend available; otherwise today's DS4/X360 ViGEm path
  (unchanged fallback). `ResolvedTarget::Ds5` is added.

### Host inside a Duo seat: `DuoDs5` (pulsar-core `input/duo.rs` + `input/ds5/duo.rs`)

Inside a Duo multi-seat (non-console WinStation) session a USB/IP pad is a system-wide
"console" device that Duo's HID isolation hides from the seat, so the host (desktop
`ds5/host_duo.rs`) creates the DualSense through Duo's `DuoController.dll` instead (type 2;
same detection as the other pads: `duo_pads_wanted()`, `PULSAR_PAD_BACKEND=duo|vigem`).
No USB/IP fallback inside a Duo seat. HID only: triggers, LEDs, rumble, gyro, touchpad;
no haptics/speaker/mic (Duo's device has no audio), so only `Ds5Output` goes to the client.

- Input: `SendReport(handle, *mut [u8; 63])` = USB input 0x01 without the id; the DLL
  rewrites seq (`[6]`), both sensor timestamps (`0x1b`, `0x30`) and zero battery bytes.
- Output: the DLL's callback carries only the motors, so a reader thread maps the driver's
  4 KiB section `Global\Duo_<parent ROOT devnode id, '\'→'_'>_output` (event
  `<same>_event`, polled every 8 ms too). The parent id comes from `CM_Get_Parent` of the
  pad's HID interface (`pid_0ce6&duocontroller`), polled up to 5 s after create. Layout:
  `shm[0]==0x02 && shm[1]==0x02` → body at `shm[2]` (HidD_SetOutputReport), else body at
  `shm[1]` with 46 bytes (WriteFile; the lightbar's blue byte is lost). Without the section
  it falls back to the callback rumble. Hardware check: `examples/duo_ds5.rs`.

### Client: `ds5::Pad` (pulsar-core `input/ds5/pad.rs`, used by desktop `controllers.rs`)

- Opens the physical pad with hidapi (SDL3's `SDL_hid_*`, so no second HID library). It
  owns input and output for that device while passthrough is on.
- Switches BT pads into enhanced mode (feature 0x05 read), reads input 0x31 (BT) / 0x01
  (USB), normalizes to the USB 0x01 layout, and sends it raw as
  `InputEvent::Ds5Input { slot, report }` (~250 Hz cap, coalesced to the input send rate).
  It also keeps feeding `GamepadState` to the UI nav / non-DS5 hosts, derived from the same
  report.
- Applies host output:
  - `Ds5Output` (raw USB 0x02 payload): USB → write as is; BT → rewrap as 0x31 (seq
    nibble, tag, CRC32 seed 0xA2). Client-side toggles mask fields (triggers, LEDs, rumble,
    audio routing).
  - Haptics PCM: BT → 0x32 reports (3 kHz s8 stereo frames + CRC); USB → upsample to 48 kHz
    and play on the pad's own audio device channels 3/4 (SDL3 audio, 4ch).
  - Speaker: BT → Opus-encoded 0x3x speaker reports (as ZerkSense5 does); USB → pad's
    audio device channels 1/2.
- SDL conflict: SDL's PS5 HIDAPI driver also writes output reports (lightbar, rumble,
  player LED) with its own BT sequence numbers. While `ds5::Pad` owns a device, SDL must
  not write to it (hint `SDL_HINT_JOYSTICK_HIDAPI_PS5` off for that device, or read it
  through SDL's generic path). **To verify in step 6.**
- Foreign bridges: if ZerkSense5/DSX/DS5Dongle already present the pad as a wired
  DualSense, the client simply sees a USB DualSense and uses the USB path. If one of them
  is running and holds the BT pad, local bridge mode refuses to start ("başka bir DualSense
  aracı çalışıyor") instead of fighting it. Detection: known process names plus a DS5 whose
  0x09 MAC is locally administered.

### Wire (additive; JSON session messages + RTP)

- Capability: host advertises `FEAT_DS5` ("ds5") only if the backend can attach (driver
  present, port free). The client sends DS5 events only to such hosts; an old peer ignores
  the unknown variants.
- `InputEvent::Ds5Input { slot: u8, report: Vec<u8> }`: client → host, USB-layout input
  report (63 bytes after the report id).
- `DataMsg::Ds5Output { slot: u8, report: Vec<u8> }`: host → client, raw USB 0x02 payload
  (47 bytes). It is sent on change plus a 500 ms refresh, because session datagrams can be
  lost and output reports are state.
- Haptics: a new RTP stream, **PT 99**, 3 kHz stereo s8 in 64-byte frames (one BT 0x32
  frame = 10.67 ms), per slot via the RTP SSRC (slot in the low byte). About 6 KB/s. Lost
  frames are dropped, not repaired; haptics are better late-free than complete. The host
  resamples 48 kHz → 3 kHz once (a low-pass FIR, ported from ZerkSense5 `AudioHaptics`).
- Speaker: reuse the Opus/RTP path with a second SSRC per slot (the "pad speaker" stream),
  mono or stereo as the game sends it.
- Mic: client → host Opus/RTP (PT 97, pad-mic SSRC) → decoded into the virtual mic iso IN.
- `DataMsg::Ds5Caps { slot, transport: Bt|Usb, features: u32 }`: client → host, so the
  host can skip work the client turned off (for example no haptics resampling when
  haptics are off).

### BT budget and "Otomatik"

- About 94 haptics reports/s (142 B each), about 13 KB/s, plus output 0x31 and speaker
  Opus. Fine on most adapters. The risk is shared 2.4 GHz Wi-Fi/BT combo chips (Pi
  onboard).
- The client measures write latency/failures per report (hidapi write time, input report
  gaps). Under sustained trouble it sheds load in this order: speaker → mic → haptics
  (→ classic rumble emulation from haptics energy). Each step shows a toast. It recovers
  after a quiet period. The same spirit as `adapt`.
- Settings (client, per feature): Adaptif tetik · HD titreşim · Kol hoparlörü · Kol
  mikrofonu · Işık çubuğu/LED, each `Otomatik | Açık | Kapalı`. A global "DualSense
  özelliklerini kullan" switch turns the whole passthrough off (today's DS4 path).

### Local bridge mode

The same `VirtualDs5` + `ds5::Pad` pair, wired back-to-back in-process: input → virtual
pad, virtual pad's output/haptics/speaker → physical pad. Toggle in Settings → Kontroller
("Bluetooth DualSense'i kablolu gibi göster"). Started by the desktop app's tray process,
so it keeps working while no session is up.

## Install

- Windows: bundle the usbip-win2 installer (x64 and ARM64, per arch) next to ViGEmBus in
  `scripts/fetch-drivers.mjs` + NSIS (silent `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`; no
  reboot was needed on the test PC). Optional HidHide is **not** bundled in v1: the
  client-side real pad lives on the client, and for local bridge mode we can hide it
  later. Games may see two pads in local bridge mode until then (doc + toggle).
- Linux: the .deb/.rpm load `vhci-hcd`, create a `pulsar-vhci` group for the host's attach
  and ship a `uaccess` hidraw rule for the client. Details: "Linux" below.
- macOS client: nothing to install.

## Linux

Verified on the Orange Pi 5B (Ubuntu 22.04, Rockchip 5.10.110 kernel) on 2026-11-11. The
client half needs a paired BT pad and is not tested yet.

### Host: attach without the usbip CLI

`usbip attach` does only three things, and the app can do them itself (libusbip
`usbip_vhci_attach_device2`):

1. Connect a TCP socket to the server and run `OP_REQ_IMPORT` on it.
2. Pick a free port: an `hs` row with state 4 in `/sys/devices/platform/vhci_hcd.0/status`.
3. Write `"<port> <sockfd> <devid> <speed>"` to `.../vhci_hcd.0/attach`, where
   `devid = busnum << 16 | devnum` and full speed = 2.

The kernel looks the fd up in the writing process and keeps its own reference, so ours is
closed right after the write. To unplug, write the port to `.../detach`. `input/ds5/vhci.rs`
does exactly that. `attach::method` picks it on Linux unless an explicit usbip path is
configured.

Why not the CLI: Ubuntu's `/usr/bin/usbip` is a per-kernel wrapper from `linux-tools`. On
vendor kernels the real binary does not exist (on the Pi: "usbip not found for kernel
5.10.110-rockchip"). The sysfs path needs nothing but the module.

**Privilege: udev rule + group (chosen).** The kernel checks only the file mode: `attach`
and `detach` are root-only `0200`, and `vhci_sysfs.c` has no `capable()` check.
`70-pulsar-vhci.rules` gives both files to the `pulsar-vhci` group with `g+w`. It runs on
the platform device's `add|bind` uevent; `bind` comes after probe has created the
attributes. Alternatives weighed:

| Option | Verdict |
| --- | --- |
| udev rule, `pulsar-vhci` group, in-process sysfs write | **Chosen.** No helper binary and no prompt per session. Works on a kiosk without a polkit agent. Cost: each user is added once (`sudo usermod -aG pulsar-vhci $USER`, then log in again). |
| pkexec + polkit action + a tiny root helper that imports and writes `attach` | More moving parts: the helper, a polkit agent, and `allow_active` fails for a kiosk session that logind does not mark active. The helper could allow only `127.0.0.1` and 054c:0ce6. But the server it talks to is the unprivileged user's own process, so that limit adds no real security. |
| `usbip` CLI + sudoers NOPASSWD | The CLI is missing on vendor kernels, and sudoers argument wildcards are easy to get wrong. |

Security: a `pulsar-vhci` member may plug any virtual USB device into this machine. That
equals physical access to a USB port (a malicious device can target kernel drivers), and
every option above grants the same. Membership is opt-in per user, never `uaccess`. Detach
is just as open: a member can unplug other vhci imports (denial of service only).

Kernel quirk: kernels built with `CONFIG_USBIP_DEBUG` (the Rockchip vendor kernel) set
`usbip_debug_flag` to all ones by default. They then log every URB (~1000 lines/s), and the
probe messages scroll out of dmesg. The package sets `options usbip-core
usbip_debug_flag=0`.

What `hid-playstation` does with the virtual pad (Pi, 5.10 backport):

- It binds: `playstation 0003:054C:0CE6.000F: hidraw0: USB HID v1.11 Gamepad`.
- It reads features 0x09 (20 B), 0x20 (64 B) and 0x05 (41 B) and accepts all three:
  "Registered DualSense controller hw_version=0x00000811 fw_version=0x0110002a", plus a
  battery `ps-controller-battery-<mac>`.
- It creates the gamepad, motion-sensor and touchpad evdev nodes.
- It writes two output reports at probe (player LED + lightbar setup). The host forwards
  them to the client's pad, like a game's.
- It rejects a second device with the same 0x09 MAC (`ps_devices_list_add`, -EEXIST).
  That is why the MAC is a per-client/slot `local_mac`.

`snd-usb-audio` creates the 4-channel card. `speaker-test -c 4 -s 3` (and `-s 4`) on it
produced ~94 haptics frames/s on the virtual pad. PulseAudio opens the new card for a few
seconds after it appears.

Any local SDL app (Steam, a running Pulsar client) sees the virtual pad as a normal
DualSense. It may open it and write output reports: on the Pi, the kiosk Pulsar read
0x09/0x20 again. On Linux the virtual pad can be told apart by its sysfs path
(`/sys/devices/platform/vhci_hcd.0/...`), like the devnode-parent check on Windows.

### Client: hidraw access

SDL3's hidapi uses hidraw. `70-pulsar-ds5.rules` sets `MODE=0660, TAG+="uaccess"` on the
hidraw nodes of the DualSense (054c:0ce6) and the Edge (054c:0df2). It matches by USB ids
and by HID name (`*054C:0CE6*`, which is how a BT pad shows up). `udevadm test` on the
virtual pad's hidraw matched both lines and gave the ACL `user:<seat user>:rw-`. Many
systems already get this from Steam's `60-steam-input.rules` (0ce6 only).
`70-xpadneo-disable-hidraw.rules` matches only `DRIVERS=="xpadneo"` (Xbox pads), so it does
not touch DualSense nodes.

`hid-playstation` stays bound on the client and writes output reports of its own:

- at probe (lightbar + player LEDs);
- on LED class writes (`/sys/class/leds/*`, e.g. a desktop's battery or player indicator);
- on evdev force feedback from local apps.

Every DualSense output report carries valid-flag bits, and the pad applies only the flagged
fields. So these writes change only the lightbar, LEDs or classic rumble they flag. They do
not cancel our trigger effects, haptics (BT 0x32) or audio. When something on the client
writes the LED class, expect the lightbar or player LEDs to jump to the kernel's colour.
The host's next state refresh (`STATE_REFRESH`, 500 ms) restores the game's colour.

### Packaging

| Installed path | Source (pulsar-desktop) |
| --- | --- |
| `/usr/lib/udev/rules.d/70-pulsar-ds5.rules` | `src-tauri/linux/70-pulsar-ds5.rules` |
| `/usr/lib/udev/rules.d/70-pulsar-vhci.rules` | `src-tauri/linux/70-pulsar-vhci.rules` |
| `/usr/lib/modules-load.d/pulsar.conf` (`vhci-hcd`) | `src-tauri/linux/modules-load-pulsar.conf` |
| `/usr/lib/modprobe.d/pulsar.conf` (debug flag off) | `src-tauri/linux/modprobe-pulsar.conf` |
| Post-install: `groupadd --system pulsar-vhci`, `modprobe vhci-hcd`, udev reload + trigger | `src-tauri/linux/postinst.sh` |

These are wired through `tauri.linux.conf.json` (`deb`/`rpm` `files` +
`postInstallScript`). The release .deb/.rpm and the Orange Pi .deb
(`scripts/opi5-image/build-deb.sh`) all get them. The Pi image adds the kiosk user to
`pulsar-vhci` (`customize.sh`).

An AppImage cannot install system files. For DualSense passthrough, copy the files above by
hand (from the repo) and run the post-install steps once:

```bash
sudo install -m644 70-pulsar-ds5.rules 70-pulsar-vhci.rules /etc/udev/rules.d/
echo vhci-hcd | sudo tee /etc/modules-load.d/pulsar.conf
echo 'options usbip-core usbip_debug_flag=0' | sudo tee /etc/modprobe.d/pulsar.conf
sudo groupadd --system pulsar-vhci && sudo usermod -aG pulsar-vhci "$USER"
sudo modprobe vhci-hcd && sudo udevadm control --reload-rules && sudo udevadm trigger
```

## Licensing

Pulsar stays GPLv3. Protocol knowledge and code ported from ZerkSense5 (MIT) carry
attribution in a header comment. Anything ported from SAxense / DS5Dongle (MPL-2.0) keeps
its MPL notice per file; MPL-2.0 files are GPL-compatible. No Sony SDK code or firmware.

## Plan

0. This doc.
1. `input/ds5/` protocol: report layouts, CRC32, BT 0x31/0x32 builders, USB↔BT input
   normalization, the haptics resampler, the trigger effect block. Pure, unit-tested
   against ZerkSense5-derived vectors.
2. `VirtualDs5` USB/IP server (Windows + Linux) + attach/detach. Test with no game on the
   maintainer's PC using the `haptics-test` tool (plays ch 3/4 on the virtual pad).
3. Local bridge mode (desktop). First end-user value; testable on this PC.
4. Wire: `FEAT_DS5`, `Ds5Input`, `Ds5Output`, `Ds5Caps`, RTP PT 99. Plus e2e tests in
   `tests/`.
5. Host integration (`EmulationTarget::Ds5`, slot → `VirtualDs5`, the default-device guard).
6. Client integration (`ds5::Pad`, the SDL ownership hand-off, USB/BT output paths,
   auto-degrade, settings UI in Turkish).
7. Installer: usbip-win2 per arch; Linux module/udev/helper.
8. Maintainer test in a real session (Windows host + this PC / Pi client), then push.

## Lessons from the first real session (2026-11-11)

- Debug builds cap the link (~14 Mbit, periodic 300-500 ms stalls): always test release.
- The session media parser has an allow-list (`media::parse`); new tags must be added there.
- WAN jitter: the client pad needs a jitter buffer (~64 ms start, 256 ms cap).
- Plugging the virtual pad at session start raced the host's audio-capture activation; the
  host now plugs it 3 s after the first report, off the serve loop.
- Wi-Fi on the client matters more than the protocol: 2.4 GHz shares the radio with BT, and a
  laptop on battery runs WLAN power saving (1.5 s stalls). Worth a client-side warning.
- Identify the virtual pad's audio endpoints by devnode, never by name ("DualSense" also
  matches a real cabled pad).

## Open questions

- SDL ownership hand-off per device (step 6).
- Linux host: `vhci-hcd` iso OUT works (Pi, 2026-11-11: ~94 haptics frames/s). A real game
  on a Linux host is untested.
- Multiple DS5 slots: one USB/IP server, one busid per slot.
- Latency budget for haptics over WAN: no jitter buffer beyond one frame; measure.

## Protocol notes

Detailed byte layouts: `notes/ds5-protocol.md` in the maintainer's worktree (to be folded
into `input/ds5/` doc comments in step 1).
