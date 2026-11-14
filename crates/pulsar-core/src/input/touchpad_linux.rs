//! DS4/DS5 touchpad-as-mouse reader for Linux clients.
//!
//! Enumerates `/dev/input/event*` for a Sony (VID `0x054C`) touchpad device
//! — specifically one whose kernel device name contains `"Touchpad"` AND
//! whose USB VID matches Sony, or whose PID matches a known DS4/DS5 PID (see
//! [`crate::input::GamepadKind::from_vid_pid`]). Reads the two multitouch
//! slots (ABS_MT_SLOT / ABS_MT_TRACKING_ID / ABS_MT_POSITION_X/Y, falling back
//! to single-touch ABS_X/Y + BTN_TOUCH) and BTN_LEFT for the physical click
//! pad, and feeds each report into [`TouchpadMouse`] — the same gesture logic
//! the Windows and macOS clients use (one finger = pointer, two = scroll,
//! press = left click, two-finger press = right click). Zero wire changes.
//!
//! **Permissions**: the calling process (or its user) must be in the `input`
//! group (or otherwise have read access to `/dev/input/event*`). If the
//! device is absent or not accessible the reader silently exits without
//! panicking or logging errors visible to the user.
//!
//! Windows and macOS read the touchpad through the desktop app's SDL3 pad
//! subsystem (or the DualSense raw-HID passthrough) instead — see
//! `pulsar-desktop/src-tauri/src/touchpad_mouse.rs`.
//!
//! # cfg gate
//! This module is `#[cfg(target_os = "linux")]` — it will not compile (or be
//! linked) on Windows or macOS.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode};

use crate::input::touchpad_mouse::{Finger, TouchFrame, TouchpadMouse, DEFAULT_SENS};
use crate::service::InputEvent;

/// Well-known Sony DS4/DS5 USB PIDs for evdev ID matching.
/// Matches the set in [`crate::input::GamepadKind::from_vid_pid`].
const SONY_VID: u16 = 0x054C;
const DS4_PIDS: &[u16] = &[0x05C4, 0x09CC, 0x0BA0];
const DS5_PIDS: &[u16] = &[0x0CE6, 0x0DF2];

/// Find the first `/dev/input/event*` device that looks like a DS4/DS5 touchpad.
///
/// Selection criteria (any one is sufficient):
/// 1. Device name contains `"Touchpad"` (case-insensitive) **and** USB VID is
///    `0x054C` (Sony).
/// 2. USB VID/PID matches any known DS4 or DS5 PID regardless of the name.
///
/// In addition, the node **must** advertise both `ABS_MT_POSITION_X` (multitouch
/// X axis) **and** `BTN_TOUCH` (finger contact). The DS4/DS5 main gamepad node
/// shares the same VID/PID and carries `ABS_X` (left stick), but it lacks
/// `ABS_MT_POSITION_X` and `BTN_TOUCH`, so it is excluded. Requiring both axes
/// prevents the gamepad node from winning the enumeration and silently starving
/// the touchpad reader (which needs `BTN_TOUCH` to set `finger_down`).
///
/// Returns `None` if no matching device is found or if `/dev/input` is
/// unreadable (e.g. missing `input` group membership).
fn find_touchpad() -> Option<Device> {
	let dir = std::fs::read_dir("/dev/input").ok()?;
	for entry in dir.flatten() {
		let path = entry.path();
		// Only consider `event*` nodes.
		if !path
			.file_name()
			.and_then(|n| n.to_str())
			.map(|n| n.starts_with("event"))
			.unwrap_or(false)
		{
			continue;
		}
		let dev = match Device::open(&path) {
			Ok(d) => d,
			Err(_) => continue,
		};
		let name = dev.name().unwrap_or("").to_lowercase();
		let id = dev.input_id();
		let vid = id.vendor();
		let pid = id.product();

		let is_sony = vid == SONY_VID;
		let is_ds4_ds5_pid =
			DS4_PIDS.contains(&pid) || DS5_PIDS.contains(&pid);
		let name_is_touchpad = name.contains("touchpad");

		if (name_is_touchpad && is_sony) || (is_sony && is_ds4_ds5_pid) {
			// Strict touchpad-node guard: require ABS_MT_POSITION_X (multitouch
			// X axis, present on the real touchpad node) AND BTN_TOUCH (finger
			// contact, also present on the touchpad node). The main gamepad node
			// has ABS_X (left stick) but NOT ABS_MT_POSITION_X + BTN_TOUCH, so
			// it is correctly excluded. Bare ABS_X acceptance was the bug:
			// enumeration order is arbitrary and the gamepad node often wins,
			// causing finger_down to never be set (no BTN_TOUCH events).
			let is_touchpad_node = dev
				.supported_absolute_axes()
				.map_or(false, |a| a.contains(AbsoluteAxisCode::ABS_MT_POSITION_X))
				&& dev
					.supported_keys()
					.map_or(false, |k| k.contains(KeyCode::BTN_TOUCH));
			if is_touchpad_node {
				return Some(dev);
			}
		}
	}
	None
}


/// Spawn a blocking reader thread that forwards DS4/DS5 touchpad gestures as
/// synthesized [`InputEvent::PointerRelative`] / [`InputEvent::Scroll`] /
/// [`InputEvent::PointerButton`] messages on `tx`.
///
/// # Parameters
/// - `tx`: an `mpsc::Sender<InputEvent>` (the same channel the gamepad reader
///   uses).  The channel is bounded in the caller; `blocking_send` is used so
///   back-pressure is respected.
/// - `running`: the session's shared `AtomicBool`; the loop exits when it
///   becomes `false`.
/// - `sens`: sensitivity multiplier (`None` → `DEFAULT_SENS`).
///
/// If the touchpad device is not found or cannot be opened the function
/// returns immediately (silent no-op) without spawning anything.
pub fn spawn_touchpad_reader(
	tx: tokio::sync::mpsc::Sender<InputEvent>,
	running: Arc<AtomicBool>,
	sens: Option<f64>,
	// Live capture gate: only forward pointer events while this returns true (the same
	// engage+focus+overlay gate the evdev keyboard/mouse capture uses). Injected as a
	// closure so pulsar-core stays free of the app's kbdhook.
	is_active: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
) {
	let sens = sens.unwrap_or(DEFAULT_SENS);
	// Try to find the device upfront; if none is available, no-op silently.
	let mut dev = match find_touchpad() {
		Some(d) => d,
		None => {
			tracing::debug!("touchpad_linux: no Sony DS4/DS5 touchpad found — reader not started");
			return;
		}
	};
	tracing::info!(
		name = dev.name().unwrap_or("<unknown>"),
		"touchpad_linux: DS4/DS5 touchpad found, starting reader"
	);

	std::thread::spawn(move || {
		let mut mouse = TouchpadMouse::new(sens);
		let mut mt = MtState::default();

		'outer: while running.load(Ordering::SeqCst) {
			// `fetch_events` blocks until at least one event is available.
			// On permission error or device removal it returns Err — exit cleanly.
			let events = match dev.fetch_events() {
				Ok(ev) => ev,
				Err(e) => {
					tracing::debug!("touchpad_linux: fetch_events error ({e}) — reader exiting");
					break 'outer;
				}
			};
			// Consult the capture gate once per batch; `TouchpadMouse` releases a held
			// click and drops the motion seed on every gate edge.
			let active = (is_active)();
			for ev in events {
				if !running.load(Ordering::SeqCst) {
					break 'outer;
				}
				match ev.destructure() {
					EventSummary::AbsoluteAxis(_, axis, value) => mt.abs(axis, value),
					EventSummary::Key(_, key, value) => mt.key(key, value),
					// EV_SYN / SYN_REPORT: one complete report.
					EventSummary::Synchronization(..) => {
						for out in mouse.update(&mt.frame(), active) {
							let _ = tx.blocking_send(out);
						}
					}
					_ => {}
				}
			}
		}
		// Never leave the host holding a button when the reader stops.
		for out in mouse.release() {
			let _ = tx.blocking_send(out);
		}
		tracing::debug!("touchpad_linux: reader thread exiting");
	});
}

/// The evdev touchpad node's state, accumulated across events and read at each
/// SYN_REPORT. Multitouch protocol B: `ABS_MT_SLOT` selects the slot the following
/// events describe, `ABS_MT_TRACKING_ID` -1 lifts that slot's finger.
#[derive(Default)]
struct MtState {
	slot: usize,
	fingers: [Finger; 2],
	/// Any multitouch event was seen → ignore the single-touch fallback axes.
	uses_mt: bool,
	/// Single-touch fallback (ABS_X/Y + BTN_TOUCH).
	st: Finger,
	click: bool,
}

impl MtState {
	fn abs(&mut self, axis: AbsoluteAxisCode, value: i32) {
		match axis {
			AbsoluteAxisCode::ABS_MT_SLOT => {
				self.uses_mt = true;
				self.slot = value.max(0) as usize;
			}
			AbsoluteAxisCode::ABS_MT_TRACKING_ID => {
				self.uses_mt = true;
				if let Some(f) = self.fingers.get_mut(self.slot) {
					f.down = value >= 0;
				}
			}
			AbsoluteAxisCode::ABS_MT_POSITION_X => {
				self.uses_mt = true;
				if let Some(f) = self.fingers.get_mut(self.slot) {
					f.x = value as f64;
				}
			}
			AbsoluteAxisCode::ABS_MT_POSITION_Y => {
				self.uses_mt = true;
				if let Some(f) = self.fingers.get_mut(self.slot) {
					f.y = value as f64;
				}
			}
			AbsoluteAxisCode::ABS_X => self.st.x = value as f64,
			AbsoluteAxisCode::ABS_Y => self.st.y = value as f64,
			_ => {}
		}
	}

	fn key(&mut self, key: KeyCode, value: i32) {
		match key {
			KeyCode::BTN_TOUCH => self.st.down = value != 0,
			// BTN_LEFT = the physical click pad.
			KeyCode::BTN_LEFT => self.click = value != 0,
			_ => {}
		}
	}

	fn frame(&self) -> TouchFrame {
		let fingers = if self.uses_mt {
			self.fingers
		} else {
			[self.st, Finger::default()]
		};
		TouchFrame { fingers, click: self.click }
	}
}
