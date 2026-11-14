//! Game controller support.
//!
//! Physical controllers are read on the **client** by the desktop app via SDL3
//! (`gilrs` was removed; this crate only holds the shared types/normalization
//! and the virtual-pad backends). The app normalizes them into a
//! transport-friendly [`GamepadState`], and the **host** replays them through a
//! [`VirtualGamepad`] backend (ViGEm on Windows, uinput on Linux, an
//! `IOHIDUserDevice` virtual HID pad on macOS).
//!
//! DualShock 3/4, DualSense, Xbox and generic pads are all supported; the
//! controller *kind* is detected from its USB vendor/product id so the host can
//! present a matching virtual device.

mod types;

/// DualSense passthrough (triggers, HD haptics, speaker/mic) — see `docs/dualsense.md`.
pub mod ds5;

/// Pen (stylus) and multi-touch: wire state types, the host injectors (Windows synthetic
/// pointer devices, Linux uinput tablet + multitouch, macOS CGEvent tablet / touch-as-mouse)
/// and the client-side gate + mouse fallback.
pub mod pen_touch;

/// macOS host: the display behind an avfoundation capture device (pointer / pen geometry).
pub mod avf_screens;

pub use types::{
	axis_to_i16, button, create_virtual_pad, create_virtual_pad_checked, create_virtual_pad_target,
	ds4_report_fields, touch_to_delta, trigger_to_u8, vid_pid_from_sdl_guid, xinput_buttons,
	ControllerInfo, EmulationTarget, GamepadKind, GamepadState, PadFailure, RecordingPad,
	ResolvedTarget, RumbleReader, VirtualGamepad,
};

/// Pure virtual-HID gamepad building blocks (report descriptors, input/feature reports,
/// rumble decoding) for the macOS `IOHIDUserDevice` backend; platform-independent so the
/// layouts are unit-tested everywhere.
pub mod virtual_hid;

/// macOS host-side virtual gamepad via `IOHIDUserDevice` — see [`mac_hid::MacHidGamepad`].
#[cfg(target_os = "macos")]
mod mac_hid;

/// Leftover virtual pads of a dead Pulsar: the pad record + pure cleanup decisions.
pub mod pad_registry;
/// Removing those leftovers (Windows SetupAPI/CfgMgr32) and recording live pads.
pub mod pad_cleanup;

/// Windows host-side virtual gamepad via **ViGEmBus** — see [`vigem::ViGEmGamepad`].
#[cfg(windows)]
mod vigem;

/// Windows host-side virtual gamepad via Duo's own controller driver (inside a Duo
/// multi-seat session) — see [`duo::DuoGamepad`].
#[cfg(windows)]
mod duo;
/// DualSense through Duo's driver (DualSense passthrough host inside a Duo seat).
#[cfg(windows)]
pub use duo::{duo_available, duo_ds5_interfaces, duo_pads_wanted, DuoDs5};

/// Linux host-side uinput backends (virtual gamepad + desktop input).
#[cfg(target_os = "linux")]
mod uinput;

/// DS4/DS5 touchpad-as-mouse reader via evdev (Linux client-side).
///
/// Enumerates `/dev/input/event*` for a Sony touchpad device and synthesizes
/// `PointerRelative` / `PointerButton` [`InputEvent`]s from raw ABS_MT / BTN
/// events. Follow-ups: Windows (raw HID via hidapi) and macOS (IOHIDManager).
#[cfg(target_os = "linux")]
pub mod touchpad_linux;

/// Windows host-side mouse + keyboard injection via `SendInput`.
#[cfg(windows)]
mod windows;

/// Windows host-side per-WINDOW mouse + keyboard injection via `PostMessage`
/// (same-host co-op: route a window-captured session's input to THAT window's
/// message pump instead of the global desktop). See [`window_win::WindowInput`]
/// for the honest ceiling (message-pump apps only, NOT DirectInput/RawInput games).
#[cfg(windows)]
mod window_win;

/// Pure helpers of the Linux/macOS per-window injectors (pointer mapping, X11
/// keysym lookup, scroll accumulation) — compiled everywhere so they are tested.
mod window_common;

/// Linux per-WINDOW injection for X11 / XWayland windows (`SendEvent`, XTest when
/// the window is in front). See [`window_x11::WindowInput`] for the ceiling (XI2
/// clients ignore synthetic core events; native Wayland windows are impossible).
#[cfg(target_os = "linux")]
mod window_x11;

/// macOS per-WINDOW injection (`CGEventPostToPid` to the window's owning process).
/// See [`window_mac::WindowInput`] for the ceiling.
#[cfg(target_os = "macos")]
mod window_mac;

/// macOS host-side mouse + keyboard injection via CoreGraphics `CGEvent`.
#[cfg(target_os = "macos")]
mod macos;

/// No-op desktop-input stub for platforms without a real backend.
#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod desktop_stub;

/// Host-side mouse + keyboard injection for remote control. Linux uses uinput
/// (works on Wayland and X11), Windows uses the Win32 `SendInput` API, and macOS
/// uses CoreGraphics `CGEvent` (the same user-mode approach Parsec uses for desktop
/// control on each OS); any remaining platform is a no-op stub.
#[cfg(target_os = "linux")]
pub use uinput::DesktopInput;

#[cfg(windows)]
pub use windows::{evdev_to_raw_key, DesktopInput, MonitorRect};

/// Per-window input injector, same API on every desktop OS: Windows = PostMessage
/// (HWND), Linux = X11 SendEvent/XTest (XID), macOS = CGEventPostToPid (CGWindowID).
/// Selected per-session by the host when the session's capture target is a specific
/// window (a launched game / picked app). Each backend documents its ceiling.
#[cfg(windows)]
pub use window_win::WindowInput;
#[cfg(target_os = "linux")]
pub use window_x11::WindowInput;
#[cfg(target_os = "macos")]
pub use window_mac::WindowInput;

#[cfg(target_os = "macos")]
pub use macos::DesktopInput;

/// macOS host: notice the local user's own mouse (listen-only CGEventTap that skips Pulsar's
/// tagged injected events) — the desktop app's "host takes the mouse back" detection.
#[cfg(target_os = "macos")]
pub mod local_pointer_macos;

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub use desktop_stub::DesktopInput;

// Controller reading + rumble now live in the app crate via SDL3 (sdl3-sys) —
// see desktop-app/src-tauri/src/controllers.rs. gilrs is gone (one library, the
// Moonlight model, and it rumbles pads with no evdev EV_FF). pulsar-core keeps only
// the shared data types (GamepadState/Kind, the VirtualGamepad host trait, helpers).
