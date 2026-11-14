//! Client side of pen / touch forwarding: one [`ClientPenTouch`] per session sits between
//! the input sources (the webview's PointerEvents, the Windows renderer's WM_POINTER) and
//! the wire.
//!
//! - A host that advertises `FEAT_PEN` / `FEAT_TOUCH` gets the real events, plus a re-send of
//!   the current state every [`KEEPALIVE`] while the pen is in range / a finger is down (the
//!   host lifts after [`SILENCE`](super::SILENCE) without one).
//! - Any other host gets the same input as mouse events ([`super::mouse`]), which is what
//!   pen and touch did before this existed.

use super::mouse::{MouseAct, PenMouse, TouchMouse};
use super::{PenState, TouchFrame, KEEPALIVE};
use crate::service::InputEvent;
use std::time::{Duration, Instant};

/// Browser-wheel pixels per screen height of two-finger scroll in the mouse fallback
/// (`InputEvent::Scroll` is in browser wheel pixels; 100 ≈ one notch on the hosts).
pub const FALLBACK_SCROLL_PX: f32 = 1500.0;

/// No new pen / touch input from the source for this long while something is still down or
/// hovering: the source lost track of it (a window that stopped getting the events without
/// a lift) — the keep-alive stops and the pen / fingers are lifted on the host.
pub const STALE: Duration = Duration::from_secs(30);

/// The same for a pen that only hovers (in range, no contact): a webview that never
/// delivers `pointerleave` when the pen leaves proximity would otherwise hold the host pen
/// "in range" — and its cursor pinned against the host user's mouse — for [`STALE`].
/// A hovering pen moves constantly, so a short limit costs nothing.
pub const STALE_HOVER: Duration = Duration::from_millis(2500);

pub struct ClientPenTouch {
	host_pen: bool,
	host_touch: bool,
	pen: PenState,
	pen_sent: Instant,
	pen_input: Instant,
	touch: TouchFrame,
	touch_sent: Instant,
	touch_input: Instant,
	pen_mouse: PenMouse,
	touch_mouse: TouchMouse,
}

fn to_events(acts: Vec<MouseAct>) -> Vec<InputEvent> {
	acts.into_iter()
		.map(|a| match a {
			MouseAct::Move { x, y } => InputEvent::PointerMotion { x: x as f64, y: y as f64 },
			MouseAct::Button { button, down } => InputEvent::PointerButton { button, down },
			MouseAct::Scroll { dx, dy } => InputEvent::Scroll {
				dx: (dx * FALLBACK_SCROLL_PX) as f64,
				dy: (dy * FALLBACK_SCROLL_PX) as f64,
			},
		})
		.collect()
}

impl ClientPenTouch {
	/// `host_pen` / `host_touch`: the host advertised `FEAT_PEN` / `FEAT_TOUCH`.
	pub fn new(host_pen: bool, host_touch: bool, now: Instant) -> Self {
		Self {
			host_pen,
			host_touch,
			pen: PenState::default(),
			pen_sent: now,
			pen_input: now,
			touch: TouchFrame::default(),
			touch_sent: now,
			touch_input: now,
			pen_mouse: PenMouse::default(),
			touch_mouse: TouchMouse::default(),
		}
	}

	/// Pass one outgoing event through: pen / touch become what this host understands,
	/// everything else is returned unchanged.
	pub fn on_event(&mut self, now: Instant, ev: InputEvent) -> Vec<InputEvent> {
		match ev {
			InputEvent::Pen(s) => {
				let s = s.sanitized();
				if self.host_pen {
					if !s.in_range && !self.pen.in_range {
						return Vec::new();
					}
					self.pen = s;
					self.pen_sent = now;
					self.pen_input = now;
					vec![InputEvent::Pen(s)]
				} else {
					to_events(self.pen_mouse.update(&s))
				}
			}
			InputEvent::Touch(f) => {
				if self.host_touch {
					if f.is_empty() && self.touch.is_empty() {
						return Vec::new();
					}
					self.touch = f;
					self.touch_sent = now;
					self.touch_input = now;
					vec![InputEvent::Touch(f)]
				} else {
					to_events(self.touch_mouse.update(now, &f))
				}
			}
			other => vec![other],
		}
	}

	/// Keep-alive re-sends and the fallback's hold timer. Call every ~50-100 ms.
	pub fn tick(&mut self, now: Instant) -> Vec<InputEvent> {
		let mut out = Vec::new();
		let pen_stale = if self.pen.contact { STALE } else { STALE_HOVER };
		if self.host_pen && self.pen.in_range && now.duration_since(self.pen_input) >= pen_stale {
			let lift = PenState { x: self.pen.x, y: self.pen.y, ..Default::default() };
			out.extend(self.on_event(now, InputEvent::Pen(lift)));
		}
		if self.host_touch && !self.touch.is_empty() && now.duration_since(self.touch_input) >= STALE {
			out.extend(self.on_event(now, InputEvent::Touch(TouchFrame::default())));
		}
		if self.host_pen && self.pen.in_range && now.duration_since(self.pen_sent) >= KEEPALIVE {
			self.pen_sent = now;
			out.push(InputEvent::Pen(self.pen));
		}
		if self.host_touch && !self.touch.is_empty() && now.duration_since(self.touch_sent) >= KEEPALIVE {
			self.touch_sent = now;
			out.push(InputEvent::Touch(self.touch));
		}
		if !self.host_touch {
			out.extend(to_events(self.touch_mouse.tick(now).0));
		}
		out
	}

	/// Lift the pen and every finger (control released, focus lost, session closing).
	pub fn release(&mut self, now: Instant) -> Vec<InputEvent> {
		let mut out = self.on_event(now, InputEvent::Pen(PenState { x: self.pen.x, y: self.pen.y, ..Default::default() }));
		out.extend(self.on_event(now, InputEvent::Touch(TouchFrame::default())));
		out
	}
}

#[cfg(test)]
mod tests {
	use super::super::TouchPoint;
	use super::*;
	use std::time::Duration;

	fn one(id: u32, x: f32, y: f32) -> TouchFrame {
		TouchFrame::new(&[TouchPoint { id, x, y, pressure: 0.0 }])
	}

	#[test]
	fn a_capable_host_gets_the_events_and_a_keepalive() {
		let t = Instant::now();
		let mut c = ClientPenTouch::new(true, true, t);
		let hover = PenState { x: 0.5, y: 0.5, in_range: true, ..Default::default() };
		assert_eq!(c.on_event(t, InputEvent::Pen(hover)), vec![InputEvent::Pen(hover)]);
		assert_eq!(c.on_event(t, InputEvent::Touch(one(1, 0.2, 0.2))).len(), 1);
		assert!(c.tick(t + Duration::from_millis(100)).is_empty());
		let again = c.tick(t + KEEPALIVE);
		assert_eq!(again, vec![InputEvent::Pen(hover), InputEvent::Touch(one(1, 0.2, 0.2))]);
		// Lifted: one final state each, then silence.
		let r = c.release(t + KEEPALIVE);
		assert_eq!(r.len(), 2);
		assert!(c.tick(t + Duration::from_secs(5)).is_empty());
		assert!(c.release(t + Duration::from_secs(5)).is_empty());
	}

	#[test]
	fn a_source_that_went_quiet_is_lifted_after_the_stale_limit() {
		let t = Instant::now();
		let mut c = ClientPenTouch::new(true, true, t);
		c.on_event(t, InputEvent::Touch(one(1, 0.2, 0.2)));
		// Keep-alives meanwhile do not count as input.
		let mut now = t;
		while now < t + STALE - Duration::from_secs(1) {
			now += KEEPALIVE;
			assert_eq!(c.tick(now).len(), 1);
		}
		assert_eq!(c.tick(t + STALE), vec![InputEvent::Touch(TouchFrame::default())]);
		assert!(c.tick(t + STALE + KEEPALIVE).is_empty());
	}

	#[test]
	fn a_hover_that_went_quiet_is_lifted_after_the_short_limit() {
		let t = Instant::now();
		let mut c = ClientPenTouch::new(true, false, t);
		let hover = PenState { x: 0.5, y: 0.5, in_range: true, ..Default::default() };
		c.on_event(t, InputEvent::Pen(hover));
		assert_eq!(c.tick(t + KEEPALIVE), vec![InputEvent::Pen(hover)]);
		let out = c.tick(t + STALE_HOVER);
		assert_eq!(out, vec![InputEvent::Pen(PenState { x: 0.5, y: 0.5, ..Default::default() })]);
		assert!(c.tick(t + STALE_HOVER + KEEPALIVE).is_empty());
		// In contact the long limit applies.
		let tip = PenState { contact: true, pressure: 0.5, ..hover };
		let t2 = t + Duration::from_secs(10);
		c.on_event(t2, InputEvent::Pen(tip));
		assert_eq!(c.tick(t2 + STALE_HOVER), vec![InputEvent::Pen(tip)]);
	}

	#[test]
	fn an_old_host_gets_mouse_events() {
		let t = Instant::now();
		let mut c = ClientPenTouch::new(false, false, t);
		let tip = PenState { x: 0.5, y: 0.25, in_range: true, contact: true, pressure: 0.3, ..Default::default() };
		assert_eq!(
			c.on_event(t, InputEvent::Pen(tip)),
			vec![
				InputEvent::PointerMotion { x: 0.5, y: 0.25 },
				InputEvent::PointerButton { button: 0, down: true }
			]
		);
		assert_eq!(
			c.release(t),
			vec![InputEvent::PointerButton { button: 0, down: false }]
		);
		// A tap.
		c.on_event(t, InputEvent::Touch(one(4, 0.1, 0.1)));
		let up = c.on_event(t, InputEvent::Touch(TouchFrame::default()));
		assert_eq!(
			up,
			vec![
				InputEvent::PointerButton { button: 0, down: true },
				InputEvent::PointerButton { button: 0, down: false }
			]
		);
		// Other events pass through untouched.
		let k = InputEvent::Key { code: 30, down: true };
		assert_eq!(c.on_event(t, k), vec![k]);
	}
}
