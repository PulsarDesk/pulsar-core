//! Multi-finger touchpad gestures (3 / 4-finger swipes and taps), host side.
//!
//! A raw multi-touch touchpad cannot be injected on a Windows or macOS host, so the client
//! sends the gesture it recognised ([`crate::service::InputEvent::Gesture`]: finger count +
//! [`GestureKind`]) and the host does **what its own user configured for that gesture**:
//!
//! - **Windows**: the precision-touchpad settings in
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad` (see [`win`]);
//! - **macOS**: the trackpad / Dock preferences (see [`mac`]);
//! - **Linux**: GNOME / KDE defaults (neither has a per-user remap of these gestures that we
//!   can read reliably; KDE's kwinrc is read for the overview effect and the desktop count,
//!   see [`linux`]).
//!
//! The settings are parsed into a [`GestureAction`] by pure, unit-tested functions; only the
//! readers ([`HostGestures::action`]) touch the OS. When the settings cannot be read, each
//! OS's factory defaults apply. The action is performed with the host's ordinary keyboard /
//! mouse injector ([`GestureAction::events`] → `Key` / `PointerButton` events), so it goes
//! wherever the session's keys go.
//!
//! Directions are the direction the **fingers** move. Desktop / Space switching follows the
//! content: fingers moving left bring in the desktop on the right (Windows, macOS, GNOME and
//! KDE all behave that way).

use serde::{Deserialize, Serialize};

use crate::service::InputEvent;

/// What the client's fingers did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GestureKind {
	SwipeUp,
	SwipeDown,
	SwipeLeft,
	SwipeRight,
	Tap,
}

/// evdev key codes used by the gesture actions (the wire's key code space).
pub mod key {
	pub const TAB: u32 = 15;
	pub const W: u32 = 17;
	pub const LEFTBRACE: u32 = 26;
	pub const RIGHTBRACE: u32 = 27;
	pub const LEFTCTRL: u32 = 29;
	pub const S: u32 = 31;
	pub const D: u32 = 32;
	pub const LEFTSHIFT: u32 = 42;
	pub const N: u32 = 49;
	pub const LEFTALT: u32 = 56;
	pub const UP: u32 = 103;
	pub const PAGEUP: u32 = 104;
	pub const LEFT: u32 = 105;
	pub const RIGHT: u32 = 106;
	pub const DOWN: u32 = 108;
	pub const PAGEDOWN: u32 = 109;
	pub const MUTE: u32 = 113;
	pub const VOLUMEDOWN: u32 = 114;
	pub const VOLUMEUP: u32 = 115;
	/// Left Win / Super / Command.
	pub const LEFTMETA: u32 = 125;
	pub const NEXTSONG: u32 = 163;
	pub const PLAYPAUSE: u32 = 164;
	pub const PREVIOUSSONG: u32 = 165;
}

/// What the host does for one gesture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GestureAction {
	/// The host user turned this gesture off (or the host OS has none for it).
	Nothing,
	/// A shortcut: press these evdev keys in order, release them in reverse.
	Chord(Vec<u32>),
	/// Click a mouse button (wire numbering: 0 left, 1 right, 2 middle).
	Click(u8),
}

impl GestureAction {
	fn chord(keys: &[u32]) -> Self {
		Self::Chord(keys.to_vec())
	}

	/// The input events that perform the action on the host.
	pub fn events(&self) -> Vec<InputEvent> {
		match self {
			Self::Nothing => Vec::new(),
			Self::Chord(keys) => {
				let mut out: Vec<InputEvent> =
					keys.iter().map(|&code| InputEvent::Key { code, down: true }).collect();
				out.extend(keys.iter().rev().map(|&code| InputEvent::Key { code, down: false }));
				out
			}
			Self::Click(button) => vec![
				InputEvent::PointerButton { button: *button, down: true },
				InputEvent::PointerButton { button: *button, down: false },
			],
		}
	}
}

/// An action plus whether it opens an overview that the opposite swipe closes again (Task
/// View, Mission Control, the GNOME / KDE overview). Those shortcuts toggle, so "swipe up,
/// then swipe down" sends the same shortcut twice — what the gesture does locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
	pub action: GestureAction,
	pub opens_overview: bool,
}

impl Mapped {
	fn plain(action: GestureAction) -> Self {
		Self { action, opens_overview: false }
	}
	fn overview(action: GestureAction) -> Self {
		Self { action, opens_overview: true }
	}
}

impl From<GestureAction> for Mapped {
	fn from(action: GestureAction) -> Self {
		Self::plain(action)
	}
}

/// Only 3- and 4-finger gestures exist; more fingers count as four.
pub fn normalize_fingers(fingers: u8) -> Option<u8> {
	match fingers {
		0..=2 => None,
		3 => Some(3),
		_ => Some(4),
	}
}

/// Windows precision touchpad (Settings → Bluetooth & devices → Touchpad → Three-/Four-finger
/// gestures, and "Advanced gestures").
pub mod win {
	use super::{key::*, GestureAction, GestureKind, Mapped};
	use std::collections::BTreeMap;

	/// Registry key (HKCU) holding the user's touchpad settings.
	pub const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad";
	pub const THREE_SLIDE: &str = "ThreeFingerSlideEnabled";
	pub const FOUR_SLIDE: &str = "FourFingerSlideEnabled";
	pub const THREE_TAP: &str = "ThreeFingerTapEnabled";
	pub const FOUR_TAP: &str = "FourFingerTapEnabled";

	/// `…SlideEnabled` values (the "Swipes" drop-down).
	pub const SLIDE_NOTHING: u32 = 0;
	pub const SLIDE_APPS: u32 = 1;
	pub const SLIDE_DESKTOPS: u32 = 2;
	pub const SLIDE_AUDIO: u32 = 3;
	/// `…TapEnabled` values (the "Taps" drop-down).
	pub const TAP_NOTHING: u32 = 0;
	pub const TAP_SEARCH: u32 = 1;
	pub const TAP_NOTIFICATIONS: u32 = 2;
	pub const TAP_PLAY_PAUSE: u32 = 3;
	pub const TAP_MIDDLE_CLICK: u32 = 4;

	/// The registry values that matter. `None` = not set → the Windows 11 default.
	#[derive(Clone, Debug, Default, PartialEq, Eq)]
	pub struct Settings {
		pub three_slide: Option<u32>,
		pub four_slide: Option<u32>,
		pub three_tap: Option<u32>,
		pub four_tap: Option<u32>,
		/// "Advanced gestures" per-gesture actions, when the user customised them (see
		/// [`advanced_action`]); a gesture listed here overrides the drop-down value.
		pub advanced: BTreeMap<(u8, GestureKind), GestureAction>,
	}

	/// Windows 11 factory defaults: three fingers switch apps, four switch desktops, a
	/// three-finger tap opens search, a four-finger tap the notification center.
	pub fn default_slide(fingers: u8) -> u32 {
		if fingers == 3 { SLIDE_APPS } else { SLIDE_DESKTOPS }
	}
	pub fn default_tap(fingers: u8) -> u32 {
		if fingers == 3 { TAP_SEARCH } else { TAP_NOTIFICATIONS }
	}

	/// What Windows does for this gesture with these settings.
	pub fn action(s: &Settings, fingers: u8, kind: GestureKind) -> Mapped {
		if let Some(a) = s.advanced.get(&(fingers, kind)) {
			// A custom Task View shortcut still toggles like the built-in one.
			let ov = *a == GestureAction::chord(&[LEFTMETA, TAB]);
			return Mapped { action: a.clone(), opens_overview: ov };
		}
		let three = fingers == 3;
		if kind == GestureKind::Tap {
			let v = if three { s.three_tap } else { s.four_tap }.unwrap_or(default_tap(fingers));
			return tap_action(v).into();
		}
		let v = if three { s.three_slide } else { s.four_slide }.unwrap_or(default_slide(fingers));
		slide_action(v, kind)
	}

	/// A "Taps" drop-down value.
	pub fn tap_action(v: u32) -> GestureAction {
		match v {
			TAP_SEARCH => GestureAction::chord(&[LEFTMETA, S]),
			// Windows 11's notification center (Windows 10's Action Center was Win+A).
			TAP_NOTIFICATIONS => GestureAction::chord(&[LEFTMETA, N]),
			TAP_PLAY_PAUSE => GestureAction::chord(&[PLAYPAUSE]),
			TAP_MIDDLE_CLICK => GestureAction::Click(2),
			_ => GestureAction::Nothing,
		}
	}

	/// A "Swipes" drop-down value for a swipe.
	pub fn slide_action(v: u32, kind: GestureKind) -> Mapped {
		use GestureKind::*;
		let c = GestureAction::chord;
		match (v, kind) {
			(SLIDE_APPS | SLIDE_DESKTOPS, SwipeUp) => Mapped::overview(c(&[LEFTMETA, TAB])),
			(SLIDE_APPS | SLIDE_DESKTOPS, SwipeDown) => c(&[LEFTMETA, D]).into(),
			// One app switch per swipe (Windows' own swipe walks the Alt+Tab list).
			(SLIDE_APPS, SwipeLeft) => c(&[LEFTALT, TAB]).into(),
			(SLIDE_APPS, SwipeRight) => c(&[LEFTALT, LEFTSHIFT, TAB]).into(),
			// The content follows the fingers: moving left brings in the desktop on the right.
			(SLIDE_DESKTOPS, SwipeLeft) => c(&[LEFTMETA, LEFTCTRL, RIGHT]).into(),
			(SLIDE_DESKTOPS, SwipeRight) => c(&[LEFTMETA, LEFTCTRL, LEFT]).into(),
			(SLIDE_AUDIO, SwipeUp) => c(&[VOLUMEUP]).into(),
			(SLIDE_AUDIO, SwipeDown) => c(&[VOLUMEDOWN]).into(),
			(SLIDE_AUDIO, SwipeLeft) => c(&[PREVIOUSSONG]).into(),
			(SLIDE_AUDIO, SwipeRight) => c(&[NEXTSONG]).into(),
			_ => GestureAction::Nothing.into(),
		}
	}

	/// The registry value names of the "Advanced gestures" page for one gesture: the action
	/// code and, for a custom shortcut, its key parameters.
	pub fn advanced_value_names(fingers: u8, kind: GestureKind) -> (String, String) {
		let n = if fingers == 3 { "Three" } else { "Four" };
		let base = match kind {
			GestureKind::SwipeUp => format!("{n}FingerUp"),
			GestureKind::SwipeDown => format!("{n}FingerDown"),
			GestureKind::SwipeLeft => format!("{n}FingerLeft"),
			GestureKind::SwipeRight => format!("{n}FingerRight"),
			GestureKind::Tap => format!("Custom{n}FingerTap"),
		};
		let params = format!("{base}KeyParams");
		(base, params)
	}

	/// An "Advanced gestures" action code (+ the custom shortcut's key parameters) → action.
	/// `None` = unknown code: the caller falls back to the drop-down value.
	pub fn advanced_action(_code: u32, _params: Option<&[u8]>) -> Option<GestureAction> {
		None
	}

	/// Windows virtual-key code → evdev key code (for custom shortcuts).
	pub fn vk_to_evdev(vk: u8) -> Option<u32> {
		Some(match vk {
			0x08 => 14,
			0x09 => 15,
			0x0D => 28,
			0x10 | 0xA0 => 42,
			0xA1 => 54,
			0x11 | 0xA2 => 29,
			0xA3 => 97,
			0x12 | 0xA4 => 56,
			0xA5 => 100,
			0x14 => 58,
			0x1B => 1,
			0x20 => 57,
			0x21 => 104,
			0x22 => 109,
			0x23 => 107,
			0x24 => 102,
			0x25 => 105,
			0x26 => 103,
			0x27 => 106,
			0x28 => 108,
			0x2C => 99,
			0x2D => 110,
			0x2E => 111,
			0x30 => 11,
			0x31..=0x39 => 2 + (vk - 0x31) as u32,
			0x41..=0x5A => return letter(vk),
			0x5B => 125,
			0x5C => 126,
			0x5D => 127,
			0x60 => 82,
			0x61 => 79,
			0x62 => 80,
			0x63 => 81,
			0x64 => 75,
			0x65 => 76,
			0x66 => 77,
			0x67 => 71,
			0x68 => 72,
			0x69 => 73,
			0x6A => 55,
			0x6B => 78,
			0x6D => 74,
			0x6E => 83,
			0x6F => 98,
			0x70..=0x79 => 59 + (vk - 0x70) as u32,
			0x7A => 87,
			0x7B => 88,
			0xAD => 113,
			0xAE => 114,
			0xAF => 115,
			0xB0 => 163,
			0xB1 => 165,
			0xB3 => 164,
			0xBA => 39,
			0xBB => 13,
			0xBC => 51,
			0xBD => 12,
			0xBE => 52,
			0xBF => 53,
			0xC0 => 41,
			0xDB => 26,
			0xDC => 43,
			0xDD => 27,
			0xDE => 40,
			_ => return None,
		})
	}

	fn letter(vk: u8) -> Option<u32> {
		// evdev codes follow the QWERTY rows, not the alphabet.
		const ROWS: [(u8, u32); 26] = [
			(b'Q', 16),
			(b'W', 17),
			(b'E', 18),
			(b'R', 19),
			(b'T', 20),
			(b'Y', 21),
			(b'U', 22),
			(b'I', 23),
			(b'O', 24),
			(b'P', 25),
			(b'A', 30),
			(b'S', 31),
			(b'D', 32),
			(b'F', 33),
			(b'G', 34),
			(b'H', 35),
			(b'J', 36),
			(b'K', 37),
			(b'L', 38),
			(b'Z', 44),
			(b'X', 45),
			(b'C', 46),
			(b'V', 47),
			(b'B', 48),
			(b'N', 49),
			(b'M', 50),
		];
		ROWS.iter().find(|(c, _)| *c == vk).map(|(_, e)| *e)
	}

	/// Read the current user's settings (HKCU of the host process — the per-user host app).
	#[cfg(windows)]
	pub fn read() -> Option<Settings> {
		let dw = |name: &str| reg_dword(KEY, name);
		let present = reg_key_exists(KEY);
		if !present {
			return None;
		}
		let mut s = Settings {
			three_slide: dw(THREE_SLIDE),
			four_slide: dw(FOUR_SLIDE),
			three_tap: dw(THREE_TAP),
			four_tap: dw(FOUR_TAP),
			advanced: BTreeMap::new(),
		};
		for fingers in [3u8, 4] {
			for kind in [
				GestureKind::SwipeUp,
				GestureKind::SwipeDown,
				GestureKind::SwipeLeft,
				GestureKind::SwipeRight,
				GestureKind::Tap,
			] {
				let (code, params) = advanced_value_names(fingers, kind);
				if let Some(code) = dw(&code) {
					let params = reg_binary(KEY, &params);
					if let Some(a) = advanced_action(code, params.as_deref()) {
						s.advanced.insert((fingers, kind), a);
					}
				}
			}
		}
		Some(s)
	}

	#[cfg(windows)]
	fn wide(s: &str) -> Vec<u16> {
		s.encode_utf16().chain(std::iter::once(0)).collect()
	}

	#[cfg(windows)]
	fn reg_key_exists(key: &str) -> bool {
		use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ};
		let k = wide(key);
		let mut h: HKEY = std::ptr::null_mut();
		let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, KEY_READ, &mut h) } == 0;
		if ok {
			unsafe { RegCloseKey(h) };
		}
		ok
	}

	#[cfg(windows)]
	fn reg_dword(key: &str, name: &str) -> Option<u32> {
		use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
		let (k, n) = (wide(key), wide(name));
		let mut v: u32 = 0;
		let mut len = std::mem::size_of::<u32>() as u32;
		let r = unsafe {
			RegGetValueW(
				HKEY_CURRENT_USER,
				k.as_ptr(),
				n.as_ptr(),
				RRF_RT_REG_DWORD,
				std::ptr::null_mut(),
				&mut v as *mut u32 as *mut core::ffi::c_void,
				&mut len,
			)
		};
		(r == 0).then_some(v)
	}

	#[cfg(windows)]
	fn reg_binary(key: &str, name: &str) -> Option<Vec<u8>> {
		use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY};
		let (k, n) = (wide(key), wide(name));
		let mut len: u32 = 0;
		let flags = RRF_RT_REG_BINARY;
		let r = unsafe {
			RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut len)
		};
		if r != 0 || len == 0 || len > 4096 {
			return None;
		}
		let mut buf = vec![0u8; len as usize];
		let r = unsafe {
			RegGetValueW(
				HKEY_CURRENT_USER,
				k.as_ptr(),
				n.as_ptr(),
				flags,
				std::ptr::null_mut(),
				buf.as_mut_ptr() as *mut core::ffi::c_void,
				&mut len,
			)
		};
		(r == 0).then(|| {
			buf.truncate(len as usize);
			buf
		})
	}
}

/// macOS trackpad (System Settings → Trackpad → More Gestures, and Mission Control).
pub mod mac {
	use super::{key::*, GestureAction, GestureKind, Mapped};

	/// The built-in trackpad's domain, then the Magic Trackpad's (System Settings writes both).
	pub const DOMAINS: [&str; 2] =
		["com.apple.AppleMultitouchTrackpad", "com.apple.driver.AppleBluetoothMultitouch.trackpad"];
	pub const THREE_HORIZ: &str = "TrackpadThreeFingerHorizSwipeGesture";
	pub const FOUR_HORIZ: &str = "TrackpadFourFingerHorizSwipeGesture";
	pub const THREE_VERT: &str = "TrackpadThreeFingerVertSwipeGesture";
	pub const FOUR_VERT: &str = "TrackpadFourFingerVertSwipeGesture";
	pub const THREE_TAP: &str = "TrackpadThreeFingerTapGesture";
	pub const THREE_DRAG: &str = "TrackpadThreeFingerDrag";
	/// `com.apple.dock` booleans: the vertical swipes open these only when enabled.
	pub const DOCK: &str = "com.apple.dock";
	pub const DOCK_MISSION_CONTROL: &str = "showMissionControlGestureEnabled";
	pub const DOCK_APP_EXPOSE: &str = "showAppExposeGestureEnabled";

	/// Horizontal swipe values: off, swipe between pages (three fingers only), swipe between
	/// full-screen apps / Spaces. Vertical: off or Mission Control / App Exposé.
	pub const OFF: i64 = 0;
	pub const PAGES: i64 = 1;
	pub const SPACES: i64 = 2;
	/// Three-finger tap: "Look up & data detectors".
	pub const TAP_LOOK_UP: i64 = 2;

	/// The preference values. `None` = not set → the macOS default.
	#[derive(Clone, Debug, Default, PartialEq, Eq)]
	pub struct Settings {
		pub three_horiz: Option<i64>,
		pub four_horiz: Option<i64>,
		pub three_vert: Option<i64>,
		pub four_vert: Option<i64>,
		pub three_tap: Option<i64>,
		pub three_drag: Option<bool>,
		pub mission_control: Option<bool>,
		pub app_expose: Option<bool>,
	}

	/// The horizontal / vertical swipe setting for a finger count, with the macOS defaults
	/// (Spaces and Mission Control on both 3 and 4 fingers; three-finger drag moves the
	/// three-finger swipes to four).
	pub fn horiz(s: &Settings, fingers: u8) -> i64 {
		if fingers == 3 { s.three_horiz.unwrap_or(SPACES) } else { s.four_horiz.unwrap_or(SPACES) }
	}
	pub fn vert(s: &Settings, fingers: u8) -> i64 {
		if fingers == 3 { s.three_vert.unwrap_or(SPACES) } else { s.four_vert.unwrap_or(SPACES) }
	}

	/// What macOS does for this gesture with these settings, as its keyboard shortcut
	/// (Mission Control ^↑, Application windows ^↓, move a Space ^← / ^→ — the defaults in
	/// System Settings → Keyboard → Shortcuts → Mission Control).
	pub fn action(s: &Settings, fingers: u8, kind: GestureKind) -> Mapped {
		use GestureKind::*;
		let c = GestureAction::chord;
		// Three-finger drag takes three fingers: a three-finger swipe is a drag there.
		if fingers == 3 && s.three_drag == Some(true) {
			return GestureAction::Nothing.into();
		}
		match kind {
			Tap => {
				// macOS has no four-finger tap.
				if fingers == 3 && s.three_tap == Some(TAP_LOOK_UP) {
					c(&[LEFTCTRL, LEFTMETA, D]).into()
				} else {
					GestureAction::Nothing.into()
				}
			}
			SwipeLeft | SwipeRight => match horiz(s, fingers) {
				SPACES if kind == SwipeLeft => c(&[LEFTCTRL, RIGHT]).into(),
				SPACES => c(&[LEFTCTRL, LEFT]).into(),
				// Three-finger page swipe: fingers to the right = back (like Safari's swipe).
				PAGES if fingers == 3 && kind == SwipeRight => c(&[LEFTMETA, LEFTBRACE]).into(),
				PAGES if fingers == 3 => c(&[LEFTMETA, RIGHTBRACE]).into(),
				_ => GestureAction::Nothing.into(),
			},
			SwipeUp | SwipeDown => {
				if vert(s, fingers) != SPACES {
					return GestureAction::Nothing.into();
				}
				if kind == SwipeUp {
					if s.mission_control == Some(false) {
						GestureAction::Nothing.into()
					} else {
						Mapped::overview(c(&[LEFTCTRL, UP]))
					}
				} else if s.app_expose == Some(false) {
					GestureAction::Nothing.into()
				} else {
					c(&[LEFTCTRL, DOWN]).into()
				}
			}
		}
	}

	/// Which finger count the user's Spaces / Mission Control swipe uses — for the macOS
	/// *client*, whose Dock swipe events do not carry a finger count. 4 when only four fingers
	/// are on (or three-finger drag is on), else 3.
	pub fn dock_swipe_fingers(s: &Settings, horizontal: bool) -> u8 {
		if s.three_drag == Some(true) {
			return 4;
		}
		let three = if horizontal { horiz(s, 3) } else { vert(s, 3) };
		if three == SPACES { 3 } else { 4 }
	}

	/// Read the current user's preferences.
	#[cfg(target_os = "macos")]
	pub fn read() -> Option<Settings> {
		let int = |key: &str| DOMAINS.iter().find_map(|d| cf::int(d, key));
		let boolean = |key: &str| DOMAINS.iter().find_map(|d| cf::boolean(d, key));
		for d in DOMAINS.iter().chain(std::iter::once(&DOCK)) {
			cf::sync(d);
		}
		Some(Settings {
			three_horiz: int(THREE_HORIZ),
			four_horiz: int(FOUR_HORIZ),
			three_vert: int(THREE_VERT),
			four_vert: int(FOUR_VERT),
			three_tap: int(THREE_TAP),
			three_drag: boolean(THREE_DRAG),
			mission_control: cf::boolean(DOCK, DOCK_MISSION_CONTROL),
			app_expose: cf::boolean(DOCK, DOCK_APP_EXPOSE),
		})
	}

	#[cfg(target_os = "macos")]
	mod cf {
		use core_foundation::base::{CFType, TCFType};
		use core_foundation::boolean::CFBoolean;
		use core_foundation::number::CFNumber;
		use core_foundation::string::{CFString, CFStringRef};
		use std::ffi::c_void;

		#[link(name = "CoreFoundation", kind = "framework")]
		unsafe extern "C" {
			fn CFPreferencesCopyAppValue(key: CFStringRef, app: CFStringRef) -> *const c_void;
			fn CFPreferencesAppSynchronize(app: CFStringRef) -> u8;
		}

		/// Drop the process's cached copy so a change made in System Settings is seen.
		pub fn sync(domain: &str) {
			let d = CFString::new(domain);
			unsafe { CFPreferencesAppSynchronize(d.as_concrete_TypeRef()) };
		}

		fn get(domain: &str, key: &str) -> Option<CFType> {
			let (d, k) = (CFString::new(domain), CFString::new(key));
			let v = unsafe { CFPreferencesCopyAppValue(k.as_concrete_TypeRef(), d.as_concrete_TypeRef()) };
			(!v.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(v) })
		}

		pub fn int(domain: &str, key: &str) -> Option<i64> {
			let v = get(domain, key)?;
			if let Some(n) = v.downcast::<CFNumber>() {
				return n.to_i64();
			}
			v.downcast::<CFBoolean>().map(|b| bool::from(b) as i64)
		}

		pub fn boolean(domain: &str, key: &str) -> Option<bool> {
			let v = get(domain, key)?;
			if let Some(b) = v.downcast::<CFBoolean>() {
				return Some(bool::from(b));
			}
			v.downcast::<CFNumber>().and_then(|n| n.to_i64()).map(|n| n != 0)
		}
	}
}

/// Linux desktops. GNOME's touchpad gestures (3 or 4 fingers: up = overview, left / right =
/// workspaces) are fixed — there is no setting to remap or disable them. KDE Plasma's are fixed
/// too (Plasma 6: up = Overview, left / right = virtual desktops); kwinrc tells whether the
/// Overview effect is enabled and how many desktops exist. Other desktops: Super opens the
/// launcher / overview where one exists.
pub mod linux {
	use super::{key::*, GestureAction, GestureKind, Mapped};

	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum Desktop {
		Gnome,
		Kde(KdeSettings),
		Other,
	}

	/// What kwinrc says (defaults when the file or the key is missing).
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub struct KdeSettings {
		/// `[Plugins] overviewEnabled` (default on).
		pub overview: bool,
		/// `[Desktops] Number` (default 1, though Plasma's wizard often makes more).
		pub desktops: Option<u32>,
	}

	impl Default for KdeSettings {
		fn default() -> Self {
			Self { overview: true, desktops: None }
		}
	}

	/// The desktop from `XDG_CURRENT_DESKTOP` (colon-separated, e.g. `ubuntu:GNOME`).
	pub fn detect(xdg_current_desktop: &str) -> Desktop {
		let parts: Vec<String> = xdg_current_desktop.split(':').map(|p| p.trim().to_ascii_uppercase()).collect();
		if parts.iter().any(|p| p == "KDE") {
			Desktop::Kde(KdeSettings::default())
		} else if parts.iter().any(|p| p == "GNOME" || p == "GNOME-CLASSIC") {
			Desktop::Gnome
		} else {
			Desktop::Other
		}
	}

	/// The kwinrc keys that matter, from the file's text (INI: `[Group]`, `key=value`).
	pub fn parse_kwinrc(text: &str) -> KdeSettings {
		let mut s = KdeSettings::default();
		let mut group = String::new();
		for line in text.lines() {
			let line = line.trim();
			if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
				group = g.to_string();
				continue;
			}
			let Some((k, v)) = line.split_once('=') else { continue };
			let (k, v) = (k.trim(), v.trim());
			match (group.as_str(), k) {
				("Plugins", "overviewEnabled") => s.overview = v != "false",
				("Desktops", "Number") => s.desktops = v.parse().ok(),
				_ => {}
			}
		}
		s
	}

	pub fn action(d: &Desktop, _fingers: u8, kind: GestureKind) -> Mapped {
		use GestureKind::*;
		let c = GestureAction::chord;
		match d {
			Desktop::Gnome => match kind {
				SwipeUp => Mapped::overview(c(&[LEFTMETA])),
				// The workspace on the right / left (GNOME 40+ lays them out horizontally;
				// Super+PageDown / PageUp is "move to workspace below / above" on older ones).
				SwipeLeft => c(&[LEFTMETA, PAGEDOWN]).into(),
				SwipeRight => c(&[LEFTMETA, PAGEUP]).into(),
				SwipeDown | Tap => GestureAction::Nothing.into(),
			},
			Desktop::Kde(k) => match kind {
				SwipeUp if k.overview => Mapped::overview(c(&[LEFTMETA, W])),
				SwipeLeft | SwipeRight if k.desktops == Some(1) => GestureAction::Nothing.into(),
				SwipeLeft => c(&[LEFTMETA, LEFTCTRL, RIGHT]).into(),
				SwipeRight => c(&[LEFTMETA, LEFTCTRL, LEFT]).into(),
				_ => GestureAction::Nothing.into(),
			},
			Desktop::Other => match kind {
				SwipeUp => c(&[LEFTMETA]).into(),
				_ => GestureAction::Nothing.into(),
			},
		}
	}

	/// The host session's desktop (+ kwinrc on KDE).
	#[cfg(target_os = "linux")]
	pub fn read() -> Desktop {
		let mut d = detect(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default());
		if let Desktop::Kde(k) = &mut d {
			let dir = std::env::var_os("XDG_CONFIG_HOME")
				.map(std::path::PathBuf::from)
				.or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")));
			if let Some(text) = dir.and_then(|d| std::fs::read_to_string(d.join("kwinrc")).ok()) {
				*k = parse_kwinrc(&text);
			}
		}
		d
	}
}

/// How long an opened overview counts as open for the closing swipe.
const OVERVIEW_OPEN_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// The host's gesture handler: reads the host user's settings on every gesture (they are a
/// handful of registry / preference / file reads, and a change in the host's Settings applies
/// at once) and remembers an overview it opened, so the opposite swipe closes it.
#[derive(Debug, Default)]
pub struct HostGestures {
	/// The overview shortcut sent by the last swipe up, and when.
	overview: Option<(GestureAction, std::time::Instant)>,
}

impl HostGestures {
	pub fn new() -> Self {
		Self::default()
	}

	/// The action for a gesture from the client, with this host's settings.
	pub fn action(&mut self, fingers: u8, kind: GestureKind) -> GestureAction {
		let Some(fingers) = normalize_fingers(fingers) else {
			return GestureAction::Nothing;
		};
		let mapped = host_mapping(fingers, kind);
		self.resolve(kind, mapped, std::time::Instant::now())
	}

	/// The toggle rule on top of the OS mapping (pure; `now` injected for the tests).
	pub fn resolve(&mut self, kind: GestureKind, mapped: Mapped, now: std::time::Instant) -> GestureAction {
		if kind == GestureKind::SwipeDown {
			if let Some((open, at)) = self.overview.take() {
				if now.duration_since(at) < OVERVIEW_OPEN_FOR {
					return open; // the shortcut toggles: send it again to close
				}
			}
		} else if kind == GestureKind::SwipeUp && mapped.opens_overview {
			if let Some((open, at)) = self.overview.take() {
				// A second swipe up: the overview stays (Windows / macOS) or goes on to the app
				// grid (GNOME: Super again would close it) — send nothing on GNOME-style toggles.
				if now.duration_since(at) < OVERVIEW_OPEN_FOR && open == mapped.action {
					self.overview = Some((open, at));
					return GestureAction::Nothing;
				}
			}
			self.overview = Some((mapped.action.clone(), now));
		} else {
			self.overview = None;
		}
		mapped.action
	}
}

/// This host's OS mapping (settings read now; factory defaults when they cannot be read).
fn host_mapping(fingers: u8, kind: GestureKind) -> Mapped {
	#[cfg(windows)]
	{
		let s = win::read().unwrap_or_default();
		win::action(&s, fingers, kind)
	}
	#[cfg(target_os = "macos")]
	{
		let s = mac::read().unwrap_or_default();
		mac::action(&s, fingers, kind)
	}
	#[cfg(target_os = "linux")]
	{
		linux::action(&linux::read(), fingers, kind)
	}
	#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
	{
		let _ = (fingers, kind);
		Mapped::plain(GestureAction::Nothing)
	}
}

#[cfg(test)]
mod tests {
	use super::key::*;
	use super::*;
	use GestureKind::*;

	fn chord(k: &[u32]) -> GestureAction {
		GestureAction::Chord(k.to_vec())
	}

	#[test]
	fn chord_presses_in_order_and_releases_in_reverse() {
		let ev = chord(&[LEFTMETA, D]).events();
		assert_eq!(
			ev,
			vec![
				InputEvent::Key { code: LEFTMETA, down: true },
				InputEvent::Key { code: D, down: true },
				InputEvent::Key { code: D, down: false },
				InputEvent::Key { code: LEFTMETA, down: false },
			]
		);
		assert_eq!(GestureAction::Click(2).events().len(), 2);
		assert!(GestureAction::Nothing.events().is_empty());
	}

	#[test]
	fn fingers_normalize() {
		assert_eq!(normalize_fingers(2), None);
		assert_eq!(normalize_fingers(3), Some(3));
		assert_eq!(normalize_fingers(4), Some(4));
		assert_eq!(normalize_fingers(5), Some(4));
	}

	#[test]
	fn windows_defaults_are_windows_11s() {
		use win::*;
		let s = Settings::default();
		let a = |f, k| action(&s, f, k).action;
		assert_eq!(a(3, SwipeUp), chord(&[LEFTMETA, TAB]));
		assert!(action(&s, 3, SwipeUp).opens_overview);
		assert_eq!(a(3, SwipeDown), chord(&[LEFTMETA, D]));
		assert_eq!(a(3, SwipeLeft), chord(&[LEFTALT, TAB]));
		assert_eq!(a(3, SwipeRight), chord(&[LEFTALT, LEFTSHIFT, TAB]));
		assert_eq!(a(3, Tap), chord(&[LEFTMETA, S]));
		assert_eq!(a(4, SwipeUp), chord(&[LEFTMETA, TAB]));
		assert_eq!(a(4, SwipeDown), chord(&[LEFTMETA, D]));
		assert_eq!(a(4, SwipeLeft), chord(&[LEFTMETA, LEFTCTRL, RIGHT]));
		assert_eq!(a(4, SwipeRight), chord(&[LEFTMETA, LEFTCTRL, LEFT]));
		assert_eq!(a(4, Tap), chord(&[LEFTMETA, N]));
	}

	#[test]
	fn windows_honours_the_users_drop_downs() {
		use win::*;
		// This laptop's settings (2026-11-15): 3 fingers = desktops, 4 = apps, 3-tap = middle.
		let s = Settings {
			three_slide: Some(SLIDE_DESKTOPS),
			four_slide: Some(SLIDE_APPS),
			three_tap: Some(TAP_MIDDLE_CLICK),
			four_tap: None,
			..Default::default()
		};
		assert_eq!(action(&s, 3, SwipeLeft).action, chord(&[LEFTMETA, LEFTCTRL, RIGHT]));
		assert_eq!(action(&s, 4, SwipeLeft).action, chord(&[LEFTALT, TAB]));
		assert_eq!(action(&s, 3, Tap).action, GestureAction::Click(2));
		assert_eq!(action(&s, 4, Tap).action, chord(&[LEFTMETA, N]));
		let audio = Settings { three_slide: Some(SLIDE_AUDIO), three_tap: Some(TAP_PLAY_PAUSE), ..Default::default() };
		assert_eq!(action(&audio, 3, SwipeUp).action, chord(&[VOLUMEUP]));
		assert!(!action(&audio, 3, SwipeUp).opens_overview);
		assert_eq!(action(&audio, 3, SwipeDown).action, chord(&[VOLUMEDOWN]));
		assert_eq!(action(&audio, 3, SwipeLeft).action, chord(&[PREVIOUSSONG]));
		assert_eq!(action(&audio, 3, SwipeRight).action, chord(&[NEXTSONG]));
		assert_eq!(action(&audio, 3, Tap).action, chord(&[PLAYPAUSE]));
		let off = Settings {
			three_slide: Some(SLIDE_NOTHING),
			three_tap: Some(TAP_NOTHING),
			four_slide: Some(77),
			..Default::default()
		};
		for k in [SwipeUp, SwipeDown, SwipeLeft, SwipeRight, Tap] {
			assert_eq!(action(&off, 3, k).action, GestureAction::Nothing, "{k:?}");
		}
		assert_eq!(action(&off, 4, SwipeUp).action, GestureAction::Nothing, "unknown value = nothing");
	}

	#[test]
	fn windows_advanced_entry_overrides_the_drop_down() {
		use win::*;
		let mut s = Settings::default();
		s.advanced.insert((3, SwipeDown), chord(&[LEFTCTRL, LEFTSHIFT, 1]));
		assert_eq!(action(&s, 3, SwipeDown).action, chord(&[LEFTCTRL, LEFTSHIFT, 1]));
		assert_eq!(action(&s, 4, SwipeDown).action, chord(&[LEFTMETA, D]));
		assert_eq!(advanced_value_names(3, SwipeUp), ("ThreeFingerUp".into(), "ThreeFingerUpKeyParams".into()));
		assert_eq!(
			advanced_value_names(4, Tap),
			("CustomFourFingerTap".into(), "CustomFourFingerTapKeyParams".into())
		);
	}

	#[test]
	fn vk_codes_map_to_evdev() {
		use win::vk_to_evdev;
		assert_eq!(vk_to_evdev(0x5B), Some(LEFTMETA));
		assert_eq!(vk_to_evdev(0x44), Some(D));
		assert_eq!(vk_to_evdev(0x51), Some(16)); // Q
		assert_eq!(vk_to_evdev(0x4D), Some(50)); // M
		assert_eq!(vk_to_evdev(0x31), Some(2)); // 1
		assert_eq!(vk_to_evdev(0x30), Some(11)); // 0
		assert_eq!(vk_to_evdev(0x70), Some(59)); // F1
		assert_eq!(vk_to_evdev(0x7B), Some(88)); // F12
		assert_eq!(vk_to_evdev(0xA2), Some(LEFTCTRL));
		assert_eq!(vk_to_evdev(0x07), None);
	}

	#[test]
	fn macos_defaults() {
		use mac::*;
		let s = Settings::default();
		let a = |f, k| action(&s, f, k).action;
		for f in [3, 4] {
			assert_eq!(a(f, SwipeUp), chord(&[LEFTCTRL, UP]));
			assert_eq!(a(f, SwipeDown), chord(&[LEFTCTRL, DOWN]));
			assert_eq!(a(f, SwipeLeft), chord(&[LEFTCTRL, RIGHT]));
			assert_eq!(a(f, SwipeRight), chord(&[LEFTCTRL, LEFT]));
			assert_eq!(a(f, Tap), GestureAction::Nothing);
		}
		assert!(action(&s, 3, SwipeUp).opens_overview);
	}

	#[test]
	fn macos_honours_the_users_preferences() {
		use mac::*;
		// Spaces on four fingers, pages on three, Mission Control on four, App Exposé off.
		let s = Settings {
			three_horiz: Some(PAGES),
			four_horiz: Some(SPACES),
			three_vert: Some(OFF),
			four_vert: Some(SPACES),
			three_tap: Some(TAP_LOOK_UP),
			three_drag: Some(false),
			mission_control: Some(true),
			app_expose: Some(false),
		};
		assert_eq!(action(&s, 3, SwipeRight).action, chord(&[LEFTMETA, LEFTBRACE]));
		assert_eq!(action(&s, 3, SwipeLeft).action, chord(&[LEFTMETA, RIGHTBRACE]));
		assert_eq!(action(&s, 4, SwipeLeft).action, chord(&[LEFTCTRL, RIGHT]));
		assert_eq!(action(&s, 3, SwipeUp).action, GestureAction::Nothing);
		assert_eq!(action(&s, 4, SwipeUp).action, chord(&[LEFTCTRL, UP]));
		assert_eq!(action(&s, 4, SwipeDown).action, GestureAction::Nothing);
		assert_eq!(action(&s, 3, Tap).action, chord(&[LEFTCTRL, LEFTMETA, D]));
		assert_eq!(action(&s, 4, Tap).action, GestureAction::Nothing);
		assert_eq!(dock_swipe_fingers(&s, true), 4);
		assert_eq!(dock_swipe_fingers(&s, false), 4);
		// Pages is three-finger only: a four-finger "pages" value does nothing.
		let p = Settings { four_horiz: Some(PAGES), ..Default::default() };
		assert_eq!(action(&p, 4, SwipeLeft).action, GestureAction::Nothing);
		// Three-finger drag owns three fingers.
		let d = Settings { three_drag: Some(true), ..Default::default() };
		assert_eq!(action(&d, 3, SwipeUp).action, GestureAction::Nothing);
		assert_eq!(action(&d, 4, SwipeUp).action, chord(&[LEFTCTRL, UP]));
		assert_eq!(dock_swipe_fingers(&d, true), 4);
		assert_eq!(dock_swipe_fingers(&Settings::default(), true), 3);
	}

	#[test]
	fn linux_desktops() {
		use linux::*;
		assert_eq!(detect("ubuntu:GNOME"), Desktop::Gnome);
		assert_eq!(detect("KDE"), Desktop::Kde(KdeSettings::default()));
		assert_eq!(detect("X-Cinnamon"), Desktop::Other);
		assert_eq!(detect(""), Desktop::Other);
		let g = Desktop::Gnome;
		assert_eq!(action(&g, 3, SwipeUp).action, chord(&[LEFTMETA]));
		assert!(action(&g, 3, SwipeUp).opens_overview);
		assert_eq!(action(&g, 4, SwipeLeft).action, chord(&[LEFTMETA, PAGEDOWN]));
		assert_eq!(action(&g, 3, SwipeRight).action, chord(&[LEFTMETA, PAGEUP]));
		assert_eq!(action(&g, 3, SwipeDown).action, GestureAction::Nothing);
		let k = Desktop::Kde(KdeSettings::default());
		assert_eq!(action(&k, 4, SwipeUp).action, chord(&[LEFTMETA, W]));
		assert_eq!(action(&k, 4, SwipeLeft).action, chord(&[LEFTMETA, LEFTCTRL, RIGHT]));
		assert_eq!(action(&k, 4, SwipeRight).action, chord(&[LEFTMETA, LEFTCTRL, LEFT]));
		assert_eq!(action(&Desktop::Other, 3, SwipeUp).action, chord(&[LEFTMETA]));
		assert_eq!(action(&Desktop::Other, 3, SwipeLeft).action, GestureAction::Nothing);
	}

	#[test]
	fn kwinrc_is_parsed() {
		use linux::*;
		let s = parse_kwinrc("[Desktops]\nId_1=abc\nNumber=1\nRows=1\n\n[Plugins]\noverviewEnabled=false\n");
		assert_eq!(s, KdeSettings { overview: false, desktops: Some(1) });
		let k = Desktop::Kde(s);
		assert_eq!(action(&k, 4, SwipeUp).action, GestureAction::Nothing);
		assert_eq!(action(&k, 4, SwipeLeft).action, GestureAction::Nothing);
		assert_eq!(parse_kwinrc("garbage\n[Plugins]\n"), KdeSettings::default());
	}

	#[test]
	fn swipe_down_closes_the_overview_the_swipe_up_opened() {
		let t0 = std::time::Instant::now();
		let mut h = HostGestures::new();
		let tv = Mapped::overview(chord(&[LEFTMETA, TAB]));
		let desk = Mapped::plain(chord(&[LEFTMETA, D]));
		assert_eq!(h.resolve(SwipeUp, tv.clone(), t0), chord(&[LEFTMETA, TAB]));
		// Second swipe up while open: no toggle-close.
		assert_eq!(h.resolve(SwipeUp, tv.clone(), t0), GestureAction::Nothing);
		// Swipe down: the same shortcut again (closes), not Win+D.
		assert_eq!(h.resolve(SwipeDown, desk.clone(), t0), chord(&[LEFTMETA, TAB]));
		// Now closed: swipe down is show desktop.
		assert_eq!(h.resolve(SwipeDown, desk.clone(), t0), chord(&[LEFTMETA, D]));
		// An overview opened long ago no longer counts.
		h.resolve(SwipeUp, tv.clone(), t0);
		let late = t0 + std::time::Duration::from_secs(60);
		assert_eq!(h.resolve(SwipeDown, desk.clone(), late), chord(&[LEFTMETA, D]));
		// Any other gesture in between forgets it.
		h.resolve(SwipeUp, tv, t0);
		h.resolve(SwipeLeft, Mapped::plain(chord(&[LEFTALT, TAB])), t0);
		assert_eq!(h.resolve(SwipeDown, desk, t0), chord(&[LEFTMETA, D]));
	}
}
