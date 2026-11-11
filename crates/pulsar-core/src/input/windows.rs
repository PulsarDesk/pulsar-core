//! Windows host-side mouse + keyboard injection via the Win32 `SendInput` API
//! (the same user-mode approach Parsec uses for desktop control).

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
	SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
	KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
	MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
	MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
	MOUSEINPUT,
};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

/// Virtual-desktop geometry of the streamed monitor, used to map a normalized (0..1)
/// absolute pointer onto the correct (possibly non-primary) screen. All in virtual-desktop
/// pixels: `mon_*` is the captured monitor's rect; `virt_*` is the bounding box of every
/// monitor (== `SM_*VIRTUALSCREEN`). Without this an absolute move always lands on the
/// PRIMARY display, since `MOUSEEVENTF_ABSOLUTE` alone maps 0..65535 to the primary only.
#[derive(Clone, Copy, Debug)]
pub struct MonitorRect {
	pub mon_left: i32,
	pub mon_top: i32,
	pub mon_width: i32,
	pub mon_height: i32,
	pub virt_left: i32,
	pub virt_top: i32,
	pub virt_width: i32,
	pub virt_height: i32,
}

const WHEEL_DELTA: i32 = 120;

fn send_mouse(dx: i32, dy: i32, data: i32, flags: u32) {
	let input = INPUT {
		r#type: INPUT_MOUSE,
		Anonymous: INPUT_0 {
			mi: MOUSEINPUT {
				dx,
				dy,
				mouseData: data as u32, // wheel delta is signed; SendInput reads the bits back as i32
				dwFlags: flags,
				time: 0,
				dwExtraInfo: 0,
			},
		},
	};
	unsafe { SendInput(1, &input, core::mem::size_of::<INPUT>() as i32) };
}

fn send_key(vk: u16, flags: u32) {
	use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
		MapVirtualKeyW, KEYEVENTF_EXTENDEDKEY, MAPVK_VK_TO_VSC_EX,
	};
	// Carry the real scan code (+ the extended flag for E0/E1 keys: right Ctrl/Alt, Win,
	// arrows, Ins/Del/Home/End/PgUp/PgDn, ...), like RustDesk's enigo does. A VK-only event
	// (wScan = 0) is not a complete keystroke: shell shortcuts such as Alt+Tab and many
	// games look at the scan code and ignored our injected keys.
	let ext = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
	let scan = (ext & 0xFF) as u16;
	let mut flags = flags;
	// The E0 prefix from MAPVK_VK_TO_VSC_EX is not reliable: on a real host (2026-11-10) it
	// came back as a bare 0x4D for VK_RIGHT, so the arrows went out WITHOUT the extended flag,
	// i.e. as the numpad 4/6/8/2 keys — Steam Big Picture scrolled instead of moving the
	// selection. The extended keys are fixed by the keyboard hardware, so list them (the same
	// set Sunshine uses) and keep the map's prefix as a second source.
	if matches!(ext >> 8, 0xE0 | 0xE1) || is_extended_vk(vk) {
		flags |= KEYEVENTF_EXTENDEDKEY;
	}
	let input = INPUT {
		r#type: INPUT_KEYBOARD,
		Anonymous: INPUT_0 {
			ki: KEYBDINPUT {
				wVk: vk,
				wScan: scan,
				dwFlags: flags,
				time: 0,
				dwExtraInfo: 0,
			},
		},
	};
	unsafe { SendInput(1, &input, core::mem::size_of::<INPUT>() as i32) };
}

/// A client evdev keycode as a full Windows keystroke: `(virtual key, scan code, extended)` —
/// the same table + scan/E0 resolution the `SendInput` path uses. For injectors that build their
/// own keyboard events (same-host co-op feeds them to one game instance as raw input).
pub fn evdev_to_raw_key(code: u32) -> Option<(u16, u16, bool)> {
	use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};
	let vk = evdev_to_vk(code)?;
	let ext = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
	let extended = matches!(ext >> 8, 0xE0 | 0xE1) || is_extended_vk(vk);
	Some((vk, (ext & 0xFF) as u16, extended))
}

/// Virtual keys whose scan code carries the E0 prefix on a PC keyboard: right Ctrl/Alt, Win,
/// Apps, the Ins/Del/Home/End/PgUp/PgDn block, the arrows, numpad /, NumLock, PrintScreen.
/// (Numpad Enter is extended too but has no VK of its own.) Pure + testable.
pub(crate) fn is_extended_vk(vk: u16) -> bool {
	matches!(
		vk,
		0xA3 // VK_RCONTROL
			| 0xA5 // VK_RMENU
			| 0x5B | 0x5C // VK_LWIN, VK_RWIN
			| 0x5D // VK_APPS
			| 0x2D | 0x2E // VK_INSERT, VK_DELETE
			| 0x24 | 0x23 // VK_HOME, VK_END
			| 0x21 | 0x22 // VK_PRIOR, VK_NEXT
			| 0x25..=0x28 // VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN
			| 0x6F // VK_DIVIDE
			| 0x90 // VK_NUMLOCK
			| 0x2C // VK_SNAPSHOT
	)
}

/// Map a Linux evdev keycode (what the client sends — see `keymap.ts`) to a
/// Windows virtual-key code. Pure + testable.
///
/// `pub(crate)` so the per-window `PostMessage` injector (`window_win.rs`) reuses
/// the exact same evdev→VK table the global `SendInput` path uses.
pub(crate) fn evdev_to_vk(code: u32) -> Option<u16> {
	Some(match code {
		1 => 0x1B,                          // Escape
		2..=10 => 0x31 + (code - 2) as u16, // Digit1..9 → '1'..'9'
		11 => 0x30,                         // Digit0 → '0'
		12 => 0xBD,                         // Minus  (VK_OEM_MINUS)
		13 => 0xBB,                         // Equal  (VK_OEM_PLUS)
		14 => 0x08,                         // Backspace
		15 => 0x09,                         // Tab
		16 => 0x51,
		17 => 0x57,
		18 => 0x45,
		19 => 0x52,
		20 => 0x54, // Q W E R T
		21 => 0x59,
		22 => 0x55,
		23 => 0x49,
		24 => 0x4F,
		25 => 0x50, // Y U I O P
		26 => 0xDB, // [  (VK_OEM_4)
		27 => 0xDD, // ]  (VK_OEM_6)
		28 => 0x0D, // Enter
		29 => 0xA2, // Left Ctrl
		30 => 0x41,
		31 => 0x53,
		32 => 0x44,
		33 => 0x46,
		34 => 0x47,
		35 => 0x48, // A S D F G H
		36 => 0x4A,
		37 => 0x4B,
		38 => 0x4C, // J K L
		39 => 0xBA, // ;  (VK_OEM_1)
		40 => 0xDE, // '  (VK_OEM_7)
		41 => 0xC0, // `  (VK_OEM_3)
		42 => 0xA0, // Left Shift
		43 => 0xDC, // \  (VK_OEM_5)
		44 => 0x5A,
		45 => 0x58,
		46 => 0x43,
		47 => 0x56,
		48 => 0x42,
		49 => 0x4E,
		50 => 0x4D,                           // Z X C V B N M
		51 => 0xBC,                           // ,  (VK_OEM_COMMA)
		52 => 0xBE,                           // .  (VK_OEM_PERIOD)
		53 => 0xBF,                           // /  (VK_OEM_2)
		54 => 0xA1,                           // Right Shift
		55 => 0x6A,                           // Numpad *  (VK_MULTIPLY)
		56 => 0xA4,                           // Left Alt  (VK_LMENU)
		57 => 0x20,                           // Space
		58 => 0x14,                           // Caps Lock
		59..=68 => 0x70 + (code - 59) as u16, // F1..F10
		69 => 0x90,                           // Num Lock
		70 => 0x91,                           // Scroll Lock
		71 => 0x67,
		72 => 0x68,
		73 => 0x69, // Numpad 7 8 9
		74 => 0x6D, // Numpad -  (VK_SUBTRACT)
		75 => 0x64,
		76 => 0x65,
		77 => 0x66, // Numpad 4 5 6
		78 => 0x6B, // Numpad +  (VK_ADD)
		79 => 0x61,
		80 => 0x62,
		81 => 0x63,  // Numpad 1 2 3
		82 => 0x60,  // Numpad 0
		83 => 0x6E,  // Numpad .  (VK_DECIMAL)
		87 => 0x7A,  // F11
		88 => 0x7B,  // F12
		96 => 0x0D,  // Numpad Enter → Return
		97 => 0xA3,  // Right Ctrl
		98 => 0x6F,  // Numpad /  (VK_DIVIDE)
		100 => 0xA5, // Right Alt (VK_RMENU)
		102 => 0x24, // Home
		103 => 0x26, // Arrow Up
		104 => 0x21, // Page Up   (VK_PRIOR)
		105 => 0x25, // Arrow Left
		106 => 0x27, // Arrow Right
		107 => 0x23, // End
		108 => 0x28, // Arrow Down
		109 => 0x22, // Page Down (VK_NEXT)
		110 => 0x2D, // Insert
		111 => 0x2E, // Delete
		125 => 0x5B, // Left Win
		126 => 0x5C, // Right Win
		127 => 0x5D, // Context Menu (VK_APPS)
		_ => return None,
	})
}

/// Injects mouse + keyboard onto the host desktop via Win32 `SendInput`.
/// SendInput targets the calling process's interactive desktop, which for the
/// user-launched Pulsar app is the seat the host is sharing. Held buttons/keys
/// are tracked so they can be released on Drop — otherwise a client that drops
/// mid-press leaves a button/modifier stuck down (e.g. a runaway drag-select).
pub struct DesktopInput {
	held_buttons: std::collections::HashSet<u8>,
	held_keys: std::collections::HashSet<u16>,
	/// Carried-over fractional scroll (in wheel notches). Fine/precision wheel deltas
	/// (< one notch) used to `round()` to 0 and do nothing; we accumulate the remainder
	/// so successive small scrolls eventually move (and big deltas behave as before).
	scroll_acc_v: f64,
	scroll_acc_h: f64,
	/// Geometry of the streamed monitor within the virtual desktop. `None` ⇒ the primary
	/// display (single-monitor host / unknown geometry) — keeps the legacy primary-only
	/// absolute mapping. `Some` ⇒ map the normalized coords onto that monitor's rect across
	/// the whole virtual screen so a non-primary monitor's clicks land on it.
	monitor: Option<MonitorRect>,
	/// Lazily started touch-pinch worker (see `pinch`).
	pinch_tx: Option<std::sync::mpsc::Sender<PinchCmd>>,
}

/// Commands for the touch-pinch worker thread.
enum PinchCmd {
	Begin,
	Scale(f64),
	End,
}

/// Finger spread (px) at pinch begin, and the clamp for the spread while pinching.
const PINCH_D0: f64 = 300.0;
const PINCH_MIN: f64 = 40.0;
const PINCH_MAX: f64 = 3000.0;

/// Two synthetic touch contacts around the cursor whose spread follows the client's pinch
/// scale (`InjectTouchInput`, Windows 8+), so host apps see a REAL pinch gesture (browsers,
/// Photos, Maps, PDF viewers) rather than Ctrl + wheel. The worker re-injects every ~16 ms:
/// it smooths the client's step-sized updates into continuous finger motion, and keeps the
/// contacts alive (unrefreshed injected touch is canceled by the system).
fn pinch_worker(rx: std::sync::mpsc::Receiver<PinchCmd>) {
	use std::sync::mpsc::RecvTimeoutError;
	use std::time::{Duration, Instant};
	use windows_sys::Win32::Foundation::RECT;
	use windows_sys::Win32::UI::Input::Pointer::{
		InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
		POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_TOUCH_INFO,
		TOUCH_FEEDBACK_NONE,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetSystemMetrics, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
		SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
	};
	if unsafe { InitializeTouchInjection(2, TOUCH_FEEDBACK_NONE) } == 0 {
		tracing::warn!(err = %std::io::Error::last_os_error(), "touch injection unavailable — pinch ignored");
		for _ in rx {}
		return;
	}
	let inject = |center: POINT, d: f64, flags: u32| {
		let (vx, vy) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) };
		let (vw, vh) = unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
		let clamp_x = |x: i32| x.clamp(vx + 4, vx + vw.max(9) - 5);
		let y = center.y.clamp(vy + 4, vy + vh.max(9) - 5);
		let half = (d / 2.0).round() as i32;
		let xs = [clamp_x(center.x - half), clamp_x(center.x + half)];
		let mut c: [POINTER_TOUCH_INFO; 2] = unsafe { std::mem::zeroed() };
		for (i, t) in c.iter_mut().enumerate() {
			t.pointerInfo.pointerType = PT_TOUCH;
			t.pointerInfo.pointerId = i as u32;
			t.pointerInfo.pointerFlags = flags;
			t.pointerInfo.ptPixelLocation = POINT { x: xs[i], y };
			t.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
			t.rcContact = RECT { left: xs[i] - 4, top: y - 4, right: xs[i] + 4, bottom: y + 4 };
			t.orientation = 90;
			t.pressure = 32000;
		}
		unsafe { InjectTouchInput(2, c.as_ptr()) }
	};
	const MOVE: u32 = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
	let mut active = false;
	let mut center = POINT { x: 0, y: 0 };
	let (mut d, mut target) = (PINCH_D0, PINCH_D0);
	let mut last_msg = Instant::now();
	loop {
		let wait = if active { Duration::from_millis(16) } else { Duration::from_secs(3600) };
		match rx.recv_timeout(wait) {
			Ok(PinchCmd::Begin) => {
				if active {
					inject(center, d, POINTER_FLAG_UP);
				}
				unsafe { GetCursorPos(&mut center) };
				d = PINCH_D0;
				target = PINCH_D0;
				active = inject(center, d, POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT) != 0;
				if !active {
					tracing::warn!(err = %std::io::Error::last_os_error(), "pinch: touch down injection failed");
				}
				last_msg = Instant::now();
				continue;
			}
			Ok(PinchCmd::Scale(s)) => {
				target = (PINCH_D0 * s).clamp(PINCH_MIN, PINCH_MAX);
				last_msg = Instant::now();
			}
			Ok(PinchCmd::End) => {
				if active {
					inject(center, target, MOVE);
					inject(center, target, POINTER_FLAG_UP);
					active = false;
				}
				continue;
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => {
				if active {
					inject(center, d, POINTER_FLAG_UP);
				}
				return;
			}
		}
		if active {
			// A client that vanished mid-pinch must not leave two fingers on the screen.
			if last_msg.elapsed() > Duration::from_millis(1500) {
				inject(center, d, POINTER_FLAG_UP);
				active = false;
				continue;
			}
			d += (target - d) * 0.45;
			inject(center, d, MOVE);
		}
	}
}

impl DesktopInput {
	pub fn new() -> std::io::Result<Self> {
		Ok(Self {
			held_buttons: std::collections::HashSet::new(),
			held_keys: std::collections::HashSet::new(),
			scroll_acc_v: 0.0,
			scroll_acc_h: 0.0,
			monitor: None,
			pinch_tx: None,
		})
	}

	/// Touchpad pinch from the client (`InputEvent::Pinch`): phase 0 begin / 1 update / 2 end,
	/// `scale` cumulative since begin. Injected as a real two-finger touch pinch at the cursor.
	pub fn pinch(&mut self, phase: u8, scale: f64) {
		let tx = self.pinch_tx.get_or_insert_with(|| {
			let (tx, rx) = std::sync::mpsc::channel();
			let _ = std::thread::Builder::new()
				.name("pulsar-pinch".into())
				.spawn(move || pinch_worker(rx));
			tx
		});
		let cmd = match phase {
			0 => PinchCmd::Begin,
			1 => PinchCmd::Scale(if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }),
			_ => PinchCmd::End,
		};
		if tx.send(cmd).is_err() {
			self.pinch_tx = None;
		}
	}

	/// Set (or clear) the streamed monitor's virtual-desktop geometry so absolute pointer
	/// moves target the captured screen, not always the primary. Called by the host whenever
	/// the stream's `display_idx` is (re)selected.
	pub fn set_monitor(&mut self, rect: Option<MonitorRect>) {
		self.monitor = rect;
	}

	/// Move the pointer to a normalized (0..1) position on the streamed monitor. When a
	/// `MonitorRect` is set, the normalized coords are placed inside that monitor's rect and
	/// remapped across the whole virtual desktop with `MOUSEEVENTF_VIRTUALDESK` — otherwise a
	/// bare `MOUSEEVENTF_ABSOLUTE` move always lands on the PRIMARY display. With no rect set
	/// (single-monitor / unknown) it falls back to the primary-only mapping.
	pub fn pointer(&mut self, x: f64, y: f64) {
		let x = x.clamp(0.0, 1.0);
		let y = y.clamp(0.0, 1.0);
		match self.monitor {
			Some(m) if m.virt_width > 0 && m.virt_height > 0 => {
				// Absolute virtual-desktop pixel of the normalized point on the streamed monitor.
				let px = m.mon_left as f64 + x * m.mon_width as f64;
				let py = m.mon_top as f64 + y * m.mon_height as f64;
				// Normalize across the whole virtual screen (what VIRTUALDESK maps 0..65535 onto).
				let nx = (px - m.virt_left as f64) / m.virt_width as f64;
				let ny = (py - m.virt_top as f64) / m.virt_height as f64;
				let dx = (nx * 65535.0).round() as i32;
				let dy = (ny * 65535.0).round() as i32;
				send_mouse(
					dx,
					dy,
					0,
					MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE,
				);
			}
			_ => {
				let dx = (x * 65535.0).round() as i32;
				let dy = (y * 65535.0).round() as i32;
				send_mouse(dx, dy, 0, MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE);
			}
		}
	}

	/// Move the pointer by a raw relative delta (native renderer / games). No
	/// ABSOLUTE flag → SendInput treats dx/dy as a relative move.
	pub fn pointer_relative(&mut self, dx: f64, dy: f64) {
		send_mouse(dx.round() as i32, dy.round() as i32, 0, MOUSEEVENTF_MOVE);
		// Confine the cursor to the STREAMED monitor. A relative move spans the whole virtual
		// desktop, so without this the pointer drifts off the captured screen onto another monitor
		// (or past its edges) and the remote user "loses" the cursor outside the shared area. Snap
		// it back inside the monitor's rect after the move. Raw-input games read the delta from the
		// move itself; SetCursorPos does NOT emit a raw delta, so this stays invisible to them. With
		// no MonitorRect (single-monitor / unknown geometry) the OS already confines to the one
		// screen, so the clamp is skipped.
		if let Some(m) = self.monitor {
			if m.mon_width > 0 && m.mon_height > 0 {
				unsafe {
					let mut p = POINT { x: 0, y: 0 };
					if GetCursorPos(&mut p) != 0 {
						let cx = p.x.clamp(m.mon_left, m.mon_left + m.mon_width - 1);
						let cy = p.y.clamp(m.mon_top, m.mon_top + m.mon_height - 1);
						if cx != p.x || cy != p.y {
							SetCursorPos(cx, cy);
						}
					}
				}
			}
		}
	}

	/// Press/release a mouse button (0=left, 1=right, 2=middle).
	pub fn button(&mut self, button: u8, down: bool) {
		if down {
			self.held_buttons.insert(button);
		} else {
			self.held_buttons.remove(&button);
		}
		let flag = match (button, down) {
			(1, true) => MOUSEEVENTF_RIGHTDOWN,
			(1, false) => MOUSEEVENTF_RIGHTUP,
			(2, true) => MOUSEEVENTF_MIDDLEDOWN,
			(2, false) => MOUSEEVENTF_MIDDLEUP,
			(_, true) => MOUSEEVENTF_LEFTDOWN,
			(_, false) => MOUSEEVENTF_LEFTUP,
		};
		send_mouse(0, 0, 0, flag);
	}

	/// Scroll by a delta (browser wheel pixels → wheel notches).
	pub fn scroll(&mut self, dx: f64, dy: f64) {
		// Accumulate fractional notches so fine/precision scroll (< 100px) isn't lost to
		// rounding; carry the sub-notch remainder to the next call (trunc toward zero
		// preserves direction). browser down(+) → wheel down (negative), so negate dy.
		self.scroll_acc_v += -dy / 100.0;
		self.scroll_acc_h += dx / 100.0;
		let v = self.scroll_acc_v.trunc() as i32;
		let h = self.scroll_acc_h.trunc() as i32;
		self.scroll_acc_v -= v as f64;
		self.scroll_acc_h -= h as f64;
		if v != 0 {
			send_mouse(0, 0, v * WHEEL_DELTA, MOUSEEVENTF_WHEEL);
		}
		if h != 0 {
			send_mouse(0, 0, h * WHEEL_DELTA, MOUSEEVENTF_HWHEEL);
		}
	}

	/// Press/release a key by evdev keycode.
	pub fn key(&mut self, code: u32, down: bool) {
		if let Some(vk) = evdev_to_vk(code) {
			if down {
				self.held_keys.insert(vk);
			} else {
				self.held_keys.remove(&vk);
			}
			send_key(vk, if down { 0 } else { KEYEVENTF_KEYUP });
		}
	}

	/// Type a resolved Unicode character verbatim (layout-independent). The client mapped a
	/// keypress through ITS keyboard layout to this codepoint; `KEYEVENTF_UNICODE` injects the
	/// exact char so Windows inserts it regardless of the host's active layout (a Turkish-Q
	/// client typing `ç` lands `ç` on a US-layout host). One-shot down+up, no held state —
	/// the host's own key-repeat is bypassed (the client re-sends `Char` on evdev autorepeat).
	pub fn type_char(&mut self, c: char) {
		let mut buf = [0u16; 2];
		for unit in c.encode_utf16(&mut buf) {
			for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
				let input = INPUT {
					r#type: INPUT_KEYBOARD,
					Anonymous: INPUT_0 {
						ki: KEYBDINPUT {
							wVk: 0, // MUST be 0 for KEYEVENTF_UNICODE
							wScan: *unit,
							dwFlags: flags,
							time: 0,
							dwExtraInfo: 0,
						},
					},
				};
				unsafe { SendInput(1, &input, core::mem::size_of::<INPUT>() as i32) };
			}
		}
	}

	/// Release every currently-held mouse button and key. Called when control is
	/// revoked mid-press ("Sadece izleme"): the gate then drops all later events,
	/// including the matching key-up/button-up, so without this flush a held
	/// Shift/Ctrl or a held mouse button (drag-select) stays stuck on the host.
	/// Idempotent — drained sets make repeat calls no-ops.
	pub fn flush_held(&mut self) {
		if let Some(tx) = &self.pinch_tx {
			let _ = tx.send(PinchCmd::End); // never leave injected fingers down
		}
		for b in self.held_buttons.drain() {
			let flag = match b {
				1 => MOUSEEVENTF_RIGHTUP,
				2 => MOUSEEVENTF_MIDDLEUP,
				_ => MOUSEEVENTF_LEFTUP,
			};
			send_mouse(0, 0, 0, flag);
		}
		for vk in self.held_keys.drain() {
			send_key(vk, KEYEVENTF_KEYUP);
		}
	}
}

impl Drop for DesktopInput {
	/// Release anything still held when the session tears down, so a mid-press
	/// disconnect can't leave the host with a stuck mouse button (→ a runaway
	/// black drag-select rectangle) or a stuck Ctrl/Alt/Shift.
	fn drop(&mut self) {
		self.flush_held();
	}
}

#[cfg(test)]
mod tests {
	use super::{evdev_to_vk, is_extended_vk};

	#[test]
	fn arrows_and_nav_block_are_extended_but_numpad_is_not() {
		// evdev Left/Up/Right/Down → VK → extended (else they go out as numpad 4/8/6/2).
		for code in [105, 103, 106, 108, 102, 107, 104, 109, 110, 111] {
			let vk = evdev_to_vk(code).unwrap();
			assert!(is_extended_vk(vk), "evdev {code} → vk {vk:#x} must be extended");
		}
		for vk in [0x64u16, 0x66, 0x68, 0x62, 0xA2, 0xA4, 0x41, 0x0D] {
			assert!(!is_extended_vk(vk), "vk {vk:#x} must not be extended");
		}
	}

	#[test]
	fn maps_common_evdev_keys_to_vk() {
		assert_eq!(evdev_to_vk(30), Some(0x41)); // A
		assert_eq!(evdev_to_vk(28), Some(0x0D)); // Enter
		assert_eq!(evdev_to_vk(57), Some(0x20)); // Space
		assert_eq!(evdev_to_vk(2), Some(0x31)); // Digit1
		assert_eq!(evdev_to_vk(11), Some(0x30)); // Digit0
		assert_eq!(evdev_to_vk(59), Some(0x70)); // F1
		assert_eq!(evdev_to_vk(68), Some(0x79)); // F10
		assert_eq!(evdev_to_vk(103), Some(0x26)); // Arrow Up
		assert_eq!(evdev_to_vk(29), Some(0xA2)); // Left Ctrl
		assert_eq!(evdev_to_vk(0), None);
		assert_eq!(evdev_to_vk(250), None);
	}
}
