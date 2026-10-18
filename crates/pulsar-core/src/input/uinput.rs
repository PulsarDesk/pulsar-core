//! Linux host-side backends built on **uinput**: the virtual [`UinputGamepad`]
//! and the mouse + keyboard injector [`DesktopInput`]. Both work on Wayland and
//! X11 (kernel input layer), unlike `x11grab`/`xdotool`.

use super::{button, GamepadKind, GamepadState, ResolvedTarget, VirtualGamepad};
use evdev::{
	uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent,
	InputId, KeyCode, PropType, RelativeAxisCode, UinputAbsSetup,
};

/// Maps our normalized button bits to evdev gamepad key codes (used by the
/// Linux backend; pure so it's testable).
fn evdev_buttons() -> [(u32, evdev::KeyCode); 15] {
	use evdev::KeyCode;
	[
		(button::A, KeyCode::BTN_SOUTH),
		(button::B, KeyCode::BTN_EAST),
		(button::X, KeyCode::BTN_WEST),
		(button::Y, KeyCode::BTN_NORTH),
		(button::LB, KeyCode::BTN_TL),
		(button::RB, KeyCode::BTN_TR),
		(button::BACK, KeyCode::BTN_SELECT),
		(button::START, KeyCode::BTN_START),
		(button::GUIDE, KeyCode::BTN_MODE),
		(button::L3, KeyCode::BTN_THUMBL),
		(button::R3, KeyCode::BTN_THUMBR),
		(button::DPAD_UP, KeyCode::BTN_DPAD_UP),
		(button::DPAD_DOWN, KeyCode::BTN_DPAD_DOWN),
		(button::DPAD_LEFT, KeyCode::BTN_DPAD_LEFT),
		(button::DPAD_RIGHT, KeyCode::BTN_DPAD_RIGHT),
	]
}

pub struct UinputGamepad {
	kind: GamepadKind,
	dev: VirtualDevice,
}

impl UinputGamepad {
	/// Create a virtual gamepad using `EmulationTarget::Auto` resolution (Xbox 360
	/// identity). This delegates to [`Self::new_target`] with `ResolvedTarget::Xbox360`
	/// so all existing call sites continue to work unchanged.
	pub fn new(kind: GamepadKind) -> std::io::Result<Self> {
		Self::new_target(kind, ResolvedTarget::Xbox360)
	}

	/// Create a virtual gamepad with an explicit emulation target.
	///
	/// - `ResolvedTarget::Ds4` → Sony identity: name "Pulsar Wireless Controller",
	///   VID/PID 054C:05C4 (DualShock 4 rev1), bus USB. SDL/Steam match the Sony
	///   pad from the name + VID/PID; a full DS4 axis/hat descriptor is a follow-up.
	/// - `ResolvedTarget::Xbox360` → keeps the current "Pulsar Virtual Gamepad" name
	///   with Microsoft identity (VID/PID 045E:028E, Xbox 360 controller).
	///
	/// Both targets share the same evdev_buttons() + ABS axis layout.
	pub fn new_target(kind: GamepadKind, resolved: ResolvedTarget) -> std::io::Result<Self> {
		let mut keys = AttributeSet::<KeyCode>::new();
		for (_, key) in evdev_buttons() {
			keys.insert(key);
		}
		let stick = |axis| {
			UinputAbsSetup::new(
				axis,
				AbsInfo::new(0, i16::MIN as i32, i16::MAX as i32, 16, 128, 1),
			)
		};
		let trig = |axis| UinputAbsSetup::new(axis, AbsInfo::new(0, 0, 255, 0, 0, 1));
		let (name, input_id) = match resolved {
			ResolvedTarget::Ds4 => (
				"Pulsar Wireless Controller",
				InputId::new(BusType::BUS_USB, 0x054C, 0x05C4, 0x8111),
			),
			ResolvedTarget::Xbox360 => (
				"Pulsar Virtual Gamepad",
				InputId::new(BusType::BUS_USB, 0x045E, 0x028E, 0x0114),
			),
		};
		let dev = VirtualDevice::builder()?
			.name(name)
			.input_id(input_id)
			.with_keys(&keys)?
			.with_absolute_axis(&stick(AbsoluteAxisCode::ABS_X))?
			.with_absolute_axis(&stick(AbsoluteAxisCode::ABS_Y))?
			.with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RX))?
			.with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RY))?
			.with_absolute_axis(&trig(AbsoluteAxisCode::ABS_Z))?
			.with_absolute_axis(&trig(AbsoluteAxisCode::ABS_RZ))?
			.build()?;
		Ok(Self { kind, dev })
	}
}

impl VirtualGamepad for UinputGamepad {
	fn kind(&self) -> GamepadKind {
		self.kind
	}
	fn apply(&mut self, state: &GamepadState) {
		let mut events = Vec::with_capacity(21);
		for (bit, key) in evdev_buttons() {
			let val = if state.is_pressed(bit) { 1 } else { 0 };
			events.push(InputEvent::new(EventType::KEY.0, key.code(), val));
		}
		let abs =
			|axis: AbsoluteAxisCode, v: i32| InputEvent::new(EventType::ABSOLUTE.0, axis.0, v);
		// evdev Y points down; gamepad up is +, so invert. Clamp to i16::MIN+1 BEFORE
		// negating: -(i16::MIN) = 32768 overflows the i16 range and exceeds the declared
		// AbsInfo max (32767) by one, which some consumers reject / wrap. Clamping the
		// low end to -32767 makes the negated value land exactly on the +32767 max.
		let inv_y = |v: i16| -(v.max(i16::MIN + 1) as i32);
		events.push(abs(AbsoluteAxisCode::ABS_X, state.left_x as i32));
		events.push(abs(AbsoluteAxisCode::ABS_Y, inv_y(state.left_y)));
		events.push(abs(AbsoluteAxisCode::ABS_RX, state.right_x as i32));
		events.push(abs(AbsoluteAxisCode::ABS_RY, inv_y(state.right_y)));
		events.push(abs(AbsoluteAxisCode::ABS_Z, state.left_trigger as i32));
		events.push(abs(AbsoluteAxisCode::ABS_RZ, state.right_trigger as i32));
		let _ = self.dev.emit(&events);
	}
}

/// Absolute-coordinate range for the virtual pointer; the compositor maps this
/// onto the screen, so the client's normalized 0..1 lands at the right pixel.
const ABS_MAX: i32 = 65535;

/// Injects mouse + keyboard onto the host desktop via two uinput devices — an
/// absolute pointer (maps directly to the screen) and a keyboard. Works on
/// Wayland and X11 (kernel input layer), unlike `x11grab`/`xdotool`.
pub struct DesktopInput {
	/// ABSOLUTE pointer (INPUT_PROP_DIRECT + ABS_X/Y) — used only by `pointer()` for
	/// direct-to-screen positioning (the old webview-canvas path / Wayland-less x11).
	pointer: VirtualDevice,
	/// RELATIVE mouse (REL_X/Y + wheel + buttons, NO direct/abs prop) — used by
	/// `pointer_relative()`, `button()`, `scroll()` (the native-renderer path). Buttons
	/// and relative motion MUST live on a non-DIRECT device: a single device carrying
	/// both INPUT_PROP_DIRECT/ABS and REL is classified by libinput as a touch/tablet,
	/// so its relative motion leaks through but BTN_LEFT (no BTN_TOUCH + no abs coord)
	/// is dropped — the live "cursor moves but clicks don't register" bug. Splitting the
	/// relative pointer onto its own plain-mouse device fixes it; X shares one cursor
	/// across both devices, so abs-position-then-click still lands correctly.
	mouse: VirtualDevice,
	keyboard: VirtualDevice,
	/// Carried-over fractional scroll (in wheel notches). A single small/precision
	/// wheel delta (< one notch) used to `round()` to 0 and silently do nothing; we
	/// instead accumulate the remainder so successive fine scrolls eventually move.
	scroll_acc_v: f64,
	scroll_acc_h: f64,
	/// Currently-held mouse-button evdev codes and key evdev codes. Tracked so they
	/// can be released on `flush_held()` (control revoked mid-press → "Sadece izleme")
	/// and on `Drop` (session teardown) — otherwise a held button/modifier stays stuck.
	held_buttons: std::collections::HashSet<u16>,
	held_keys: std::collections::HashSet<u16>,
}

impl DesktopInput {
	pub fn new() -> std::io::Result<Self> {
		let mut btns = AttributeSet::<KeyCode>::new();
		btns.insert(KeyCode::BTN_LEFT);
		btns.insert(KeyCode::BTN_RIGHT);
		btns.insert(KeyCode::BTN_MIDDLE);
		// ABSOLUTE pointer: INPUT_PROP_DIRECT + ABS_X/Y. Buttons stay here too so an
		// absolute-path tap (set position, then click on THIS device) still works.
		let mut props = AttributeSet::<PropType>::new();
		props.insert(PropType::DIRECT);
		let abs = |axis| UinputAbsSetup::new(axis, AbsInfo::new(0, 0, ABS_MAX, 0, 0, 1));
		let pointer = VirtualDevice::builder()?
			.name("Pulsar Virtual Pointer")
			.with_properties(&props)?
			.with_keys(&btns)?
			.with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X))?
			.with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y))?
			.build()?;

		// RELATIVE mouse: plain REL_X/Y + wheel + buttons, NO direct/abs prop, so
		// libinput classifies it as an ordinary mouse and its BTN_LEFT clicks land
		// (the native-renderer path). Kept SEPARATE from the absolute device above —
		// mixing DIRECT/ABS with REL on one node is exactly what dropped the clicks.
		let mut rels = AttributeSet::<RelativeAxisCode>::new();
		rels.insert(RelativeAxisCode::REL_X);
		rels.insert(RelativeAxisCode::REL_Y);
		rels.insert(RelativeAxisCode::REL_WHEEL);
		rels.insert(RelativeAxisCode::REL_HWHEEL);
		let mouse = VirtualDevice::builder()?
			.name("Pulsar Virtual Mouse")
			.with_keys(&btns)?
			.with_relative_axes(&rels)?
			.build()?;

		let mut keys = AttributeSet::<KeyCode>::new();
		for c in 1u16..=248 {
			keys.insert(KeyCode::new(c));
		}
		let keyboard = VirtualDevice::builder()?
			.name("Pulsar Virtual Keyboard")
			.with_keys(&keys)?
			.build()?;
		Ok(Self {
			pointer,
			mouse,
			keyboard,
			scroll_acc_v: 0.0,
			scroll_acc_h: 0.0,
			held_buttons: std::collections::HashSet::new(),
			held_keys: std::collections::HashSet::new(),
		})
	}

	/// Move the pointer to a normalized (0..1) position on the screen.
	pub fn pointer(&mut self, x: f64, y: f64) {
		let cx = (x.clamp(0.0, 1.0) * ABS_MAX as f64) as i32;
		let cy = (y.clamp(0.0, 1.0) * ABS_MAX as f64) as i32;
		let _ = self.pointer.emit(&[
			InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, cx),
			InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, cy),
		]);
	}

	/// Move the pointer by a relative delta (native renderer / games).
	pub fn pointer_relative(&mut self, dx: f64, dy: f64) {
		let mut ev = Vec::new();
		let x = dx.round() as i32;
		let y = dy.round() as i32;
		if x != 0 {
			ev.push(InputEvent::new(
				EventType::RELATIVE.0,
				RelativeAxisCode::REL_X.0,
				x,
			));
		}
		if y != 0 {
			ev.push(InputEvent::new(
				EventType::RELATIVE.0,
				RelativeAxisCode::REL_Y.0,
				y,
			));
		}
		if !ev.is_empty() {
			let _ = self.mouse.emit(&ev);
		}
	}

	/// Press/release a mouse button (0=left, 1=right, 2=middle).
	pub fn button(&mut self, button: u8, down: bool) {
		let key = match button {
			1 => KeyCode::BTN_RIGHT,
			2 => KeyCode::BTN_MIDDLE,
			_ => KeyCode::BTN_LEFT,
		};
		if down {
			self.held_buttons.insert(key.code());
		} else {
			self.held_buttons.remove(&key.code());
		}
		let _ = self
			.mouse
			.emit(&[InputEvent::new(EventType::KEY.0, key.code(), down as i32)]);
	}

	/// Scroll by a delta (browser wheel pixels → wheel clicks).
	pub fn scroll(&mut self, dx: f64, dy: f64) {
		// Accumulate fractional notches so fine/precision scroll (< 100px) isn't lost to
		// rounding. We keep the leftover sub-notch remainder for the next call; the sign
		// of the truncation matters, so use trunc() (toward zero) and carry the rest.
		// evdev wheel up is +, browser down is +, so negate the vertical delta.
		self.scroll_acc_v += -dy / 100.0;
		self.scroll_acc_h += dx / 100.0;
		let v = self.scroll_acc_v.trunc() as i32;
		let h = self.scroll_acc_h.trunc() as i32;
		self.scroll_acc_v -= v as f64;
		self.scroll_acc_h -= h as f64;
		let mut ev = Vec::new();
		if v != 0 {
			ev.push(InputEvent::new(
				EventType::RELATIVE.0,
				RelativeAxisCode::REL_WHEEL.0,
				v,
			));
		}
		if h != 0 {
			ev.push(InputEvent::new(
				EventType::RELATIVE.0,
				RelativeAxisCode::REL_HWHEEL.0,
				h,
			));
		}
		if !ev.is_empty() {
			let _ = self.mouse.emit(&ev);
		}
	}

	/// Press/release a key by evdev keycode.
	pub fn key(&mut self, code: u32, down: bool) {
		if code == 0 || code > 248 {
			return;
		}
		let code = code as u16;
		if down {
			self.held_keys.insert(code);
		} else {
			self.held_keys.remove(&code);
		}
		let _ = self
			.keyboard
			.emit(&[InputEvent::new(EventType::KEY.0, code, down as i32)]);
	}

	/// Type a resolved Unicode character (layout-independent). uinput is keycode-based with no
	/// direct Unicode-insert path, so a Linux HOST can't natively insert an arbitrary codepoint.
	/// A Linux CLIENT resolves printable keys through xkb and forwards ONLY `Char(c)` (suppressing
	/// the raw `Key` press/release), so without this the character is silently dropped on a Linux
	/// host (Windows hosts use KEYEVENTF_UNICODE). We synthesize ASCII codepoints by tapping the
	/// matching US-layout evdev keycode (+ Shift when needed) on the existing keyboard device —
	/// reliable for URLs, passwords, chat and filenames. Non-ASCII codepoints have no fixed
	/// keycode without a temporary keymap remap and remain unhandled.
	pub fn type_char(&mut self, c: char) {
		let Some((key, shift)) = us_char_to_key(c) else {
			return;
		};
		let code = key.code();
		// If Shift is already physically held by the client (tracked in held_keys), do NOT
		// emit a synthetic LEFTSHIFT down/up around the keytap. The kernel already sees Shift
		// held and will produce the shifted character on its own. Adding a trailing
		// LEFTSHIFT(0) would release the host's held Shift at the kernel/X layer even though
		// the client's Shift key is still down — the held_keys set and the actual kernel
		// modifier state would diverge, breaking subsequent Shift+Arrow / Shift+Home / etc.
		let shift_already_held = self.held_keys.contains(&KeyCode::KEY_LEFTSHIFT.code())
			|| self.held_keys.contains(&KeyCode::KEY_RIGHTSHIFT.code());
		let mut ev = Vec::with_capacity(4);
		if shift && !shift_already_held {
			ev.push(InputEvent::new(
				EventType::KEY.0,
				KeyCode::KEY_LEFTSHIFT.code(),
				1,
			));
		}
		ev.push(InputEvent::new(EventType::KEY.0, code, 1));
		ev.push(InputEvent::new(EventType::KEY.0, code, 0));
		if shift && !shift_already_held {
			ev.push(InputEvent::new(
				EventType::KEY.0,
				KeyCode::KEY_LEFTSHIFT.code(),
				0,
			));
		}
		let _ = self.keyboard.emit(&ev);
	}

	/// Release every currently-held mouse button and key. Called when control is
	/// revoked mid-press ("Sadece izleme"): the gate then drops all later events,
	/// including the matching key-up/button-up, so without this flush a held
	/// modifier or a held mouse button (drag-select) stays stuck on the host.
	/// Idempotent — drained sets make repeat calls no-ops.
	pub fn flush_held(&mut self) {
		let btn_up: Vec<InputEvent> = self
			.held_buttons
			.drain()
			.map(|code| InputEvent::new(EventType::KEY.0, code, 0))
			.collect();
		if !btn_up.is_empty() {
			let _ = self.mouse.emit(&btn_up);
		}
		let key_up: Vec<InputEvent> = self
			.held_keys
			.drain()
			.map(|code| InputEvent::new(EventType::KEY.0, code, 0))
			.collect();
		if !key_up.is_empty() {
			let _ = self.keyboard.emit(&key_up);
		}
	}
}

impl Drop for DesktopInput {
	/// Release anything still held when the session tears down, so a mid-press
	/// disconnect can't leave the host with a stuck mouse button (→ a runaway
	/// drag-select) or a stuck Ctrl/Alt/Shift.
	fn drop(&mut self) {
		self.flush_held();
	}
}

/// Maps an ASCII character to the US-layout evdev key that produces it and whether Shift is held.
/// The host keyboard device exposes the full evdev keycode range, so emitting these raw codes (the
/// same codes a Linux client's `Key` path sends) lands the character regardless of the host's own
/// X/Wayland layout. Returns `None` for non-ASCII (no fixed keycode without a keymap remap).
fn us_char_to_key(c: char) -> Option<(KeyCode, bool)> {
	let unshifted = |k: KeyCode| Some((k, false));
	let shifted = |k: KeyCode| Some((k, true));
	match c {
		'a'..='z' => unshifted(KeyCode::new(letter_key_code(c, false))),
		'A'..='Z' => shifted(KeyCode::new(letter_key_code(c, true))),
		'0' => unshifted(KeyCode::KEY_0),
		'1' => unshifted(KeyCode::KEY_1),
		'2' => unshifted(KeyCode::KEY_2),
		'3' => unshifted(KeyCode::KEY_3),
		'4' => unshifted(KeyCode::KEY_4),
		'5' => unshifted(KeyCode::KEY_5),
		'6' => unshifted(KeyCode::KEY_6),
		'7' => unshifted(KeyCode::KEY_7),
		'8' => unshifted(KeyCode::KEY_8),
		'9' => unshifted(KeyCode::KEY_9),
		' ' => unshifted(KeyCode::KEY_SPACE),
		'\t' => unshifted(KeyCode::KEY_TAB),
		'\n' | '\r' => unshifted(KeyCode::KEY_ENTER),
		'-' => unshifted(KeyCode::KEY_MINUS),
		'=' => unshifted(KeyCode::KEY_EQUAL),
		'[' => unshifted(KeyCode::KEY_LEFTBRACE),
		']' => unshifted(KeyCode::KEY_RIGHTBRACE),
		'\\' => unshifted(KeyCode::KEY_BACKSLASH),
		';' => unshifted(KeyCode::KEY_SEMICOLON),
		'\'' => unshifted(KeyCode::KEY_APOSTROPHE),
		'`' => unshifted(KeyCode::KEY_GRAVE),
		',' => unshifted(KeyCode::KEY_COMMA),
		'.' => unshifted(KeyCode::KEY_DOT),
		'/' => unshifted(KeyCode::KEY_SLASH),
		'!' => shifted(KeyCode::KEY_1),
		'@' => shifted(KeyCode::KEY_2),
		'#' => shifted(KeyCode::KEY_3),
		'$' => shifted(KeyCode::KEY_4),
		'%' => shifted(KeyCode::KEY_5),
		'^' => shifted(KeyCode::KEY_6),
		'&' => shifted(KeyCode::KEY_7),
		'*' => shifted(KeyCode::KEY_8),
		'(' => shifted(KeyCode::KEY_9),
		')' => shifted(KeyCode::KEY_0),
		'_' => shifted(KeyCode::KEY_MINUS),
		'+' => shifted(KeyCode::KEY_EQUAL),
		'{' => shifted(KeyCode::KEY_LEFTBRACE),
		'}' => shifted(KeyCode::KEY_RIGHTBRACE),
		'|' => shifted(KeyCode::KEY_BACKSLASH),
		':' => shifted(KeyCode::KEY_SEMICOLON),
		'"' => shifted(KeyCode::KEY_APOSTROPHE),
		'~' => shifted(KeyCode::KEY_GRAVE),
		'<' => shifted(KeyCode::KEY_COMMA),
		'>' => shifted(KeyCode::KEY_DOT),
		'?' => shifted(KeyCode::KEY_SLASH),
		_ => None,
	}
}

/// evdev keycode for an ASCII letter. The alphabet is NOT contiguous in evdev order, so look it up
/// in a row-ordered table (q-row, a-row, z-row) keyed by the lowercase letter.
fn letter_key_code(c: char, upper: bool) -> u16 {
	let lower = if upper {
		c.to_ascii_lowercase()
	} else {
		c
	};
	// evdev key order by physical row: top (Q..P), home (A..L), bottom (Z..M).
	const ROWS: &[(char, u16)] = &[
		('q', 16),
		('w', 17),
		('e', 18),
		('r', 19),
		('t', 20),
		('y', 21),
		('u', 22),
		('i', 23),
		('o', 24),
		('p', 25),
		('a', 30),
		('s', 31),
		('d', 32),
		('f', 33),
		('g', 34),
		('h', 35),
		('j', 36),
		('k', 37),
		('l', 38),
		('z', 44),
		('x', 45),
		('c', 46),
		('v', 47),
		('b', 48),
		('n', 49),
		('m', 50),
	];
	ROWS.iter()
		.find(|(ch, _)| *ch == lower)
		.map(|(_, code)| *code)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::{button, evdev_buttons};

	#[test]
	fn evdev_button_map_is_complete_and_correct() {
		let map = evdev_buttons();
		assert_eq!(map.len(), 15);
		assert!(map
			.iter()
			.any(|(b, k)| *b == button::A && *k == evdev::KeyCode::BTN_SOUTH));
		assert!(map
			.iter()
			.any(|(b, k)| *b == button::START && *k == evdev::KeyCode::BTN_START));
		assert!(map
			.iter()
			.any(|(b, k)| *b == button::DPAD_UP && *k == evdev::KeyCode::BTN_DPAD_UP));
	}
}
