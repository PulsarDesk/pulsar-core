//! DS4 / DualSense touchpad as a mouse — the platform-independent gesture logic.
//!
//! Every client reader (Linux evdev in `touchpad_linux`, the desktop app's SDL3 /
//! raw-HID readers on Windows and macOS) turns its device data into a [`TouchFrame`]
//! per report and feeds it to one [`TouchpadMouse`] per pad. The machine returns the
//! [`InputEvent`]s to forward to the host, so the behaviour is the same on every OS:
//!
//! - **one finger** moves the pointer (`PointerRelative`, whole counts — the fraction is
//!   carried to the next report so slow moves still add up);
//! - **two fingers** scroll (`Scroll`, natural direction: the content follows the
//!   fingers, like a phone or a macOS trackpad);
//! - **pressing the pad** clicks: left button, or right button when two fingers are on
//!   the pad at the moment of the press. The release always lifts the button the press
//!   used, even if a finger left in between.
//!
//! A change in the number of fingers re-seeds the motion (no jump when a second finger
//! lands or one lifts). When the capture gate closes (Pulsar lost focus, the session menu
//! opened) a held button is released and nothing is forwarded until it opens again.

use crate::service::InputEvent;

/// Default pointer sensitivity: screen pixels per raw touchpad unit. The DS4 / DualSense
/// pad is ~1920 units wide, so a full swipe moves ~670 px.
pub const DEFAULT_SENS: f64 = 0.35;

/// Scroll gain: wheel units (100 = one notch, the wire's browser-wheel scale) per raw
/// touchpad unit. A quarter-pad swipe (~250 units) scrolls about three notches.
pub const SCROLL_GAIN: f64 = 1.25;

/// Raw touchpad width (both DS4 and DualSense report 0..1919).
pub const PAD_WIDTH: f64 = 1920.0;
/// Raw touchpad height of a DualShock 4 (0..941).
pub const DS4_PAD_HEIGHT: f64 = 942.0;
/// Raw touchpad height of a DualSense (0..1079).
pub const DS5_PAD_HEIGHT: f64 = 1080.0;

/// One contact on the pad, in raw touchpad units.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Finger {
	pub down: bool,
	pub x: f64,
	pub y: f64,
}

/// The pad's state in one report: up to two contacts (the hardware tracks two) and
/// whether the pad itself is pressed (the click button under the surface).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TouchFrame {
	pub fingers: [Finger; 2],
	pub click: bool,
}

impl TouchFrame {
	/// Number of contacts on the pad.
	pub fn count(&self) -> usize {
		self.fingers.iter().filter(|f| f.down).count()
	}
}

/// Turn a normalized (0..1) contact — SDL's `SDL_GetGamepadTouchpadFinger` — into raw
/// touchpad units, so every reader feeds [`TouchpadMouse`] the same scale.
pub fn finger_from_normalized(down: bool, x: f32, y: f32, pad_height: f64) -> Finger {
	Finger {
		down,
		x: x.clamp(0.0, 1.0) as f64 * PAD_WIDTH,
		y: y.clamp(0.0, 1.0) as f64 * pad_height,
	}
}

/// Parse the two touch points of a DualSense input report (USB report 0x01, 64 bytes
/// including the report id) and the pad-click bit. Each point is 4 bytes at offset 33 /
/// 37: byte 0 bit 7 set = NOT touching, then a 12-bit X and a 12-bit Y. The click is
/// bit 1 of the third button byte (offset 10).
pub fn ds5_usb_touch(r: &[u8; 64]) -> TouchFrame {
	let point = |o: usize| Finger {
		down: r[o] & 0x80 == 0,
		x: (r[o + 1] as u16 | ((r[o + 2] as u16 & 0x0F) << 8)) as f64,
		y: ((r[o + 2] as u16 >> 4) | ((r[o + 3] as u16) << 4)) as f64,
	};
	TouchFrame {
		fingers: [point(33), point(37)],
		click: r[10] & 0x02 != 0,
	}
}

/// The per-pad gesture state machine. See the module docs for the gestures.
#[derive(Debug)]
pub struct TouchpadMouse {
	sens: f64,
	/// Last contact positions, valid while the finger count is unchanged.
	prev: Option<[Finger; 2]>,
	prev_count: usize,
	/// Fractional pointer motion not yet sent (whole counts only go out).
	carry: (f64, f64),
	/// Fractional scroll not yet sent (whole wheel units only go out).
	scroll_carry: (f64, f64),
	/// The mouse button a pad press is holding (0 = left, 1 = right).
	held: Option<u8>,
	/// The pad was pressed in the previous frame (edge detection).
	was_click: bool,
	/// The capture gate was open in the previous frame.
	was_active: bool,
}

impl Default for TouchpadMouse {
	fn default() -> Self {
		Self::new(DEFAULT_SENS)
	}
}

impl TouchpadMouse {
	pub fn new(sens: f64) -> Self {
		Self {
			sens,
			prev: None,
			prev_count: 0,
			carry: (0.0, 0.0),
			scroll_carry: (0.0, 0.0),
			held: None,
			was_click: false,
			was_active: false,
		}
	}

	/// Whether a pad press currently holds a host mouse button.
	pub fn holding(&self) -> bool {
		self.held.is_some()
	}

	/// Release whatever the pad holds (pad unplugged, session ending). Returns the
	/// button-up to forward, if any, and resets the motion seed.
	pub fn release(&mut self) -> Vec<InputEvent> {
		let mut out = Vec::new();
		if let Some(b) = self.held.take() {
			out.push(InputEvent::PointerButton { button: b, down: false });
		}
		self.prev = None;
		self.carry = (0.0, 0.0);
		self.scroll_carry = (0.0, 0.0);
		out
	}

	/// Feed one report. `active` is the capture gate (forward only while it is open).
	pub fn update(&mut self, frame: &TouchFrame, active: bool) -> Vec<InputEvent> {
		let mut out = Vec::new();
		if active != self.was_active {
			// Gate edge: drop a held click (no stuck host drag) and the motion seed (no
			// jump by the distance travelled while gated off).
			out.extend(self.release());
			self.was_active = active;
		}
		let count = frame.count();
		if !active {
			self.was_click = frame.click;
			self.prev_count = count;
			return out;
		}

		// Motion first, so a press lands where the finger already moved the pointer.
		if count == self.prev_count && count > 0 {
			if let Some(prev) = self.prev {
				let (mut dx, mut dy) = (0.0, 0.0);
				for (f, p) in frame.fingers.iter().zip(prev.iter()) {
					if f.down && p.down {
						dx += f.x - p.x;
						dy += f.y - p.y;
					}
				}
				let n = count as f64;
				let (dx, dy) = (dx / n, dy / n);
				if count == 1 {
					let x = self.carry.0 + dx * self.sens;
					let y = self.carry.1 + dy * self.sens;
					let (ox, oy) = (x.trunc(), y.trunc());
					self.carry = (x - ox, y - oy);
					if ox != 0.0 || oy != 0.0 {
						out.push(InputEvent::PointerRelative { dx: ox, dy: oy });
					}
				} else {
					// Natural scrolling: fingers up → content up → wheel "down" (+dy on
					// the wire). The wire's dy is browser-style (+ = scroll down).
					let x = self.scroll_carry.0 - dx * SCROLL_GAIN;
					let y = self.scroll_carry.1 - dy * SCROLL_GAIN;
					let (ox, oy) = (x.trunc(), y.trunc());
					self.scroll_carry = (x - ox, y - oy);
					if ox != 0.0 || oy != 0.0 {
						out.push(InputEvent::Scroll { dx: ox, dy: oy });
					}
				}
			}
		} else {
			// Finger count changed: re-seed (no jump) and drop stale fractions.
			self.carry = (0.0, 0.0);
			self.scroll_carry = (0.0, 0.0);
		}
		self.prev = (count > 0).then_some(frame.fingers);
		self.prev_count = count;

		// Pad press / release.
		if frame.click && !self.was_click && self.held.is_none() {
			let b = if count >= 2 { 1 } else { 0 };
			self.held = Some(b);
			out.push(InputEvent::PointerButton { button: b, down: true });
		} else if !frame.click && self.was_click {
			if let Some(b) = self.held.take() {
				out.push(InputEvent::PointerButton { button: b, down: false });
			}
		}
		self.was_click = frame.click;
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn one(x: f64, y: f64) -> TouchFrame {
		TouchFrame {
			fingers: [Finger { down: true, x, y }, Finger::default()],
			click: false,
		}
	}

	fn two(x: f64, y: f64) -> TouchFrame {
		TouchFrame {
			fingers: [Finger { down: true, x, y }, Finger { down: true, x: x + 300.0, y }],
			click: false,
		}
	}

	fn rel(ev: &[InputEvent]) -> (f64, f64) {
		ev.iter().fold((0.0, 0.0), |a, e| match e {
			InputEvent::PointerRelative { dx, dy } => (a.0 + dx, a.1 + dy),
			_ => a,
		})
	}

	#[test]
	fn first_touch_seeds_without_moving() {
		let mut m = TouchpadMouse::new(1.0);
		assert!(m.update(&one(500.0, 500.0), true).is_empty());
		assert_eq!(rel(&m.update(&one(510.0, 495.0), true)), (10.0, -5.0));
	}

	#[test]
	fn fractions_carry_over() {
		let mut m = TouchpadMouse::new(0.25);
		m.update(&one(0.0, 0.0), true);
		let mut total = 0.0;
		for i in 1..=8 {
			total += rel(&m.update(&one(i as f64, 0.0), true)).0;
		}
		assert_eq!(total, 2.0);
	}

	#[test]
	fn lift_and_retouch_does_not_jump() {
		let mut m = TouchpadMouse::new(1.0);
		m.update(&one(100.0, 100.0), true);
		m.update(&TouchFrame::default(), true);
		assert!(m.update(&one(1500.0, 800.0), true).is_empty());
	}

	#[test]
	fn two_fingers_scroll_naturally() {
		let mut m = TouchpadMouse::new(1.0);
		m.update(&two(500.0, 500.0), true);
		let ev = m.update(&two(500.0, 400.0), true);
		// Fingers moved up 100 units → +125 (scroll down, content follows the fingers).
		assert_eq!(ev, vec![InputEvent::Scroll { dx: 0.0, dy: 125.0 }]);
		assert_eq!(rel(&ev), (0.0, 0.0));
	}

	#[test]
	fn second_finger_landing_reseeds() {
		let mut m = TouchpadMouse::new(1.0);
		m.update(&one(500.0, 500.0), true);
		assert!(m.update(&two(900.0, 900.0), true).is_empty());
		assert!(m.update(&one(100.0, 100.0), true).is_empty());
	}

	#[test]
	fn click_left_and_two_finger_click_right() {
		let mut m = TouchpadMouse::new(1.0);
		let mut f = one(10.0, 10.0);
		f.click = true;
		assert_eq!(m.update(&f, true), vec![InputEvent::PointerButton { button: 0, down: true }]);
		f.click = false;
		assert_eq!(m.update(&f, true), vec![InputEvent::PointerButton { button: 0, down: false }]);
		let mut g = two(10.0, 10.0);
		m.update(&g, true);
		g.click = true;
		assert_eq!(m.update(&g, true), vec![InputEvent::PointerButton { button: 1, down: true }]);
		// One finger lifts before the release: still the right button comes up.
		let mut h = one(10.0, 10.0);
		h.click = false;
		assert_eq!(m.update(&h, true), vec![InputEvent::PointerButton { button: 1, down: false }]);
	}

	#[test]
	fn gate_close_releases_and_blocks() {
		let mut m = TouchpadMouse::new(1.0);
		let mut f = one(10.0, 10.0);
		f.click = true;
		m.update(&f, true);
		assert_eq!(m.update(&f, false), vec![InputEvent::PointerButton { button: 0, down: false }]);
		f.fingers[0].x = 500.0;
		assert!(m.update(&f, false).is_empty());
		// Gate opens with the pad still pressed: no phantom click, no jump.
		assert!(m.update(&f, true).is_empty());
		f.click = false;
		assert!(m.update(&f, true).is_empty());
	}

	#[test]
	fn ds5_report_touch_points() {
		let mut r = [0u8; 64];
		r[0] = 0x01;
		// Point 0 touching at x = 0x234, y = 0x156.
		r[33] = 0x05;
		r[34] = 0x34;
		r[35] = 0x62; // x high nibble 2, y low nibble 6
		r[36] = 0x15;
		// Point 1 not touching.
		r[37] = 0x80;
		r[10] = 0x02;
		let t = ds5_usb_touch(&r);
		assert_eq!(t.fingers[0], Finger { down: true, x: 0x234 as f64, y: 0x156 as f64 });
		assert!(!t.fingers[1].down);
		assert!(t.click);
		assert_eq!(t.count(), 1);
	}

	#[test]
	fn normalized_scales_to_raw() {
		let f = finger_from_normalized(true, 0.5, 1.0, DS4_PAD_HEIGHT);
		assert_eq!((f.x, f.y), (960.0, 942.0));
	}
}
