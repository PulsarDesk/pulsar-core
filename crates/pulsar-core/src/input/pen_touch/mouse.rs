//! Pen and touch expressed as a mouse — for the places with no real pen / touch path:
//! touch on a macOS host (there is no public touch-injection API on macOS) and a client
//! talking to a host that does not advertise `FEAT_PEN` / `FEAT_TOUCH` (old hosts).
//!
//! Touch follows the usual touchscreen conventions:
//! - one finger: the cursor jumps to it; a short tap is a left click; a finger that moves
//!   past [`TAP_SLOP`] or rests longer than [`HOLD`] presses the left button (drag);
//! - two fingers: scroll, the content following the fingers (their midpoint);
//! - a finger added to a drag, or a third finger, changes nothing.

use super::{PenState, TouchFrame};
use std::time::{Duration, Instant};

/// A mouse action in normalized screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseAct {
	/// Absolute cursor position, 0..1.
	Move { x: f32, y: f32 },
	/// 0 = left, 1 = right.
	Button { button: u8, down: bool },
	/// Scroll in fractions of the screen; positive = toward the end of the content (down /
	/// right), the same sense as a browser wheel delta.
	Scroll { dx: f32, dy: f32 },
}

/// A finger that moves this far (fraction of the screen diagonal-ish, per axis) is a drag,
/// not a tap.
pub const TAP_SLOP: f32 = 0.012;
/// A finger resting this long without moving presses the button (press-and-drag).
pub const HOLD: Duration = Duration::from_millis(180);

/// Pen → mouse: hover moves the cursor, the tip is the left button, the tip with the barrel
/// button held at touch-down is the right button.
#[derive(Clone, Debug, Default)]
pub struct PenMouse {
	down: Option<u8>,
	last: Option<(f32, f32)>,
}

impl PenMouse {
	pub fn update(&mut self, s: &PenState) -> Vec<MouseAct> {
		let s = s.sanitized();
		let mut out = Vec::new();
		if s.in_range && self.last != Some((s.x, s.y)) {
			out.push(MouseAct::Move { x: s.x, y: s.y });
			self.last = Some((s.x, s.y));
		}
		match (self.down, s.contact) {
			(None, true) => {
				let b = if s.barrel { 1 } else { 0 };
				self.down = Some(b);
				out.push(MouseAct::Button { button: b, down: true });
			}
			(Some(b), false) => {
				self.down = None;
				out.push(MouseAct::Button { button: b, down: false });
			}
			_ => {}
		}
		if !s.in_range {
			self.last = None;
		}
		out
	}
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
	Idle,
	/// One finger down, no button yet: `(id, where it landed, when)`.
	Pending { id: u32, x: f32, y: f32, at: Instant },
	/// The left button is held by finger `id`.
	Drag { id: u32 },
	/// Two-finger scroll; the last midpoint.
	Scroll { mx: f32, my: f32 },
	/// The gesture ended (or became something we do not map): wait for every finger to lift.
	Done,
}

/// Touch → mouse state machine (see the module docs). Pure: time is passed in.
#[derive(Clone, Debug)]
pub struct TouchMouse {
	mode: Mode,
}

impl Default for TouchMouse {
	fn default() -> Self {
		Self { mode: Mode::Idle }
	}
}

fn midpoint(f: &TouchFrame) -> (f32, f32) {
	let p = f.points();
	((p[0].x + p[1].x) / 2.0, (p[0].y + p[1].y) / 2.0)
}

impl TouchMouse {
	/// Feed the current set of fingers.
	pub fn update(&mut self, now: Instant, f: &TouchFrame) -> Vec<MouseAct> {
		let mut out = Vec::new();
		loop {
			match self.mode {
				Mode::Idle => {
					let Some(p) = f.points().first() else { break };
					out.push(MouseAct::Move { x: p.x, y: p.y });
					if f.len() >= 2 {
						let (mx, my) = midpoint(f);
						self.mode = Mode::Scroll { mx, my };
					} else {
						self.mode = Mode::Pending { id: p.id, x: p.x, y: p.y, at: now };
					}
					break;
				}
				Mode::Pending { id, x, y, at } => {
					let Some(p) = f.get(id).copied() else {
						// Lifted before it became a drag: a tap = a click where it landed.
						out.push(MouseAct::Button { button: 0, down: true });
						out.push(MouseAct::Button { button: 0, down: false });
						self.mode = if f.is_empty() { Mode::Idle } else { Mode::Done };
						break;
					};
					if f.len() >= 2 {
						let (mx, my) = midpoint(f);
						self.mode = Mode::Scroll { mx, my };
						break;
					}
					let moved = (p.x - x).abs() > TAP_SLOP || (p.y - y).abs() > TAP_SLOP;
					if moved || now.duration_since(at) >= HOLD {
						out.push(MouseAct::Button { button: 0, down: true });
						self.mode = Mode::Drag { id };
						continue; // the Drag arm moves the cursor to the finger
					}
					break;
				}
				Mode::Drag { id } => {
					match f.get(id) {
						Some(p) => out.push(MouseAct::Move { x: p.x, y: p.y }),
						None => {
							out.push(MouseAct::Button { button: 0, down: false });
							self.mode = if f.is_empty() { Mode::Idle } else { Mode::Done };
						}
					}
					break;
				}
				Mode::Scroll { mx, my } => {
					if f.len() < 2 {
						self.mode = if f.is_empty() { Mode::Idle } else { Mode::Done };
						break;
					}
					let (nx, ny) = midpoint(f);
					// Content follows the fingers: fingers moving down scroll toward the top.
					let (dx, dy) = (mx - nx, my - ny);
					if dx != 0.0 || dy != 0.0 {
						out.push(MouseAct::Scroll { dx, dy });
					}
					self.mode = Mode::Scroll { mx: nx, my: ny };
					break;
				}
				Mode::Done => {
					if f.is_empty() {
						self.mode = Mode::Idle;
					}
					break;
				}
			}
		}
		out
	}

	/// The hold timer: a resting finger becomes a press after [`HOLD`]. Returns the actions
	/// and when to call again.
	pub fn tick(&mut self, now: Instant) -> (Vec<MouseAct>, Option<Instant>) {
		if let Mode::Pending { id, at, .. } = self.mode {
			if now.duration_since(at) >= HOLD {
				self.mode = Mode::Drag { id };
				return (vec![MouseAct::Button { button: 0, down: true }], None);
			}
			return (Vec::new(), Some(at + HOLD));
		}
		(Vec::new(), None)
	}

	/// The left button is held by a finger (a release must reach the host on teardown).
	pub fn pressed(&self) -> bool {
		matches!(self.mode, Mode::Drag { .. })
	}
}

#[cfg(test)]
mod tests {
	use super::super::TouchPoint;
	use super::*;

	fn frame(pts: &[(u32, f32, f32)]) -> TouchFrame {
		let v: Vec<TouchPoint> = pts.iter().map(|&(id, x, y)| TouchPoint { id, x, y, pressure: 0.0 }).collect();
		TouchFrame::new(&v)
	}
	const DOWN: MouseAct = MouseAct::Button { button: 0, down: true };
	const UP: MouseAct = MouseAct::Button { button: 0, down: false };

	#[test]
	fn a_quick_tap_is_a_click_where_the_finger_landed() {
		let t = Instant::now();
		let mut m = TouchMouse::default();
		assert_eq!(m.update(t, &frame(&[(1, 0.5, 0.5)])), vec![MouseAct::Move { x: 0.5, y: 0.5 }]);
		assert_eq!(m.update(t, &frame(&[(1, 0.505, 0.5)])), vec![]);
		assert_eq!(m.update(t, &frame(&[])), vec![DOWN, UP]);
	}

	#[test]
	fn a_moving_finger_drags_and_a_resting_one_presses_after_the_hold() {
		let t = Instant::now();
		let mut m = TouchMouse::default();
		m.update(t, &frame(&[(1, 0.2, 0.2)]));
		assert_eq!(
			m.update(t, &frame(&[(1, 0.3, 0.2)])),
			vec![DOWN, MouseAct::Move { x: 0.3, y: 0.2 }]
		);
		assert!(m.pressed());
		assert_eq!(m.update(t, &frame(&[])), vec![UP]);

		let mut m = TouchMouse::default();
		m.update(t, &frame(&[(9, 0.2, 0.2)]));
		let (a, again) = m.tick(t + Duration::from_millis(50));
		assert!(a.is_empty());
		assert_eq!(again, Some(t + HOLD));
		let (a, again) = m.tick(t + HOLD);
		assert_eq!((a, again), (vec![DOWN], None));
		assert_eq!(m.update(t + HOLD, &frame(&[])), vec![UP]);
	}

	#[test]
	fn two_fingers_scroll_with_the_content_following_them() {
		let t = Instant::now();
		let mut m = TouchMouse::default();
		m.update(t, &frame(&[(1, 0.4, 0.4)]));
		assert_eq!(m.update(t, &frame(&[(1, 0.4, 0.4), (2, 0.6, 0.4)])), vec![]);
		let a = m.update(t, &frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]));
		assert_eq!(a.len(), 1);
		let MouseAct::Scroll { dx, dy } = a[0] else { panic!("{a:?}") };
		assert!(dx.abs() < 1e-6 && (dy + 0.1).abs() < 1e-6, "fingers down → scroll up: {dy}");
		// One finger lifts: the gesture is over, no click, no drag.
		assert_eq!(m.update(t, &frame(&[(2, 0.6, 0.5)])), vec![]);
		assert_eq!(m.update(t, &frame(&[(2, 0.9, 0.9)])), vec![]);
		assert_eq!(m.update(t, &frame(&[])), vec![]);
		assert!(!m.pressed());
	}

	#[test]
	fn pen_maps_tip_to_left_and_barrel_tip_to_right() {
		let mut p = PenMouse::default();
		let hover = PenState { x: 0.1, y: 0.2, in_range: true, ..Default::default() };
		assert_eq!(p.update(&hover), vec![MouseAct::Move { x: 0.1, y: 0.2 }]);
		assert_eq!(p.update(&hover), vec![]);
		let tip = PenState { contact: true, pressure: 0.4, ..hover };
		assert_eq!(p.update(&tip), vec![DOWN]);
		assert_eq!(p.update(&PenState::default()), vec![UP]);
		let side = PenState { contact: true, barrel: true, ..hover };
		assert_eq!(
			p.update(&side),
			vec![MouseAct::Move { x: 0.1, y: 0.2 }, MouseAct::Button { button: 1, down: true }]
		);
		assert_eq!(p.update(&hover), vec![MouseAct::Button { button: 1, down: false }]);
	}
}
