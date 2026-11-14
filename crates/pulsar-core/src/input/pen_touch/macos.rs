//! macOS pen + touch injection through **CoreGraphics CGEvent**.
//!
//! - **Pen**: real tablet events — a `kCGEventTabletProximity` event when the pen (or the
//!   eraser) enters / leaves range, and mouse events carrying the tablet-point subtype with
//!   pressure, tilt, rotation and the button mask, which is exactly what a Wacom driver
//!   posts. Apps that read `NSEvent.pressure` / `tilt` (Krita, Photoshop, Affinity, Preview
//!   markup) see a tablet. The barrel button held at touch-down makes it a right click.
//! - **Touch**: macOS has **no public API to inject touches** (there is no touchscreen
//!   input path at all; trackpad gestures are private `IOHIDEvent`s). Touch frames are
//!   mapped to the mouse instead ([`TouchMouse`]): tap = click, drag = left drag, two
//!   fingers = pixel scroll that follows the fingers. Pinch / rotate cannot be injected
//!   (no public magnify-gesture event).
//!
//! Positions map onto the streamed display in points (`Geom` = its CGDirectDisplayID, set by
//! `DesktopInput::set_display`; `None` = the main display), like `DesktopInput::pointer`.
//! Posting needs the Accessibility grant (see `input/macos.rs`). Untested on macOS hardware.

use super::mouse::{MouseAct, TouchMouse};
use super::{PenState, Sink, SlotChange, TouchFrame};
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::time::Instant;

/// `kCGEventMouseSubtypeTabletPoint`.
const SUBTYPE_TABLET_POINT: i64 = 1;
/// `NSPointingDeviceType`: pen / eraser.
const POINTER_PEN: i64 = 1;
const POINTER_ERASER: i64 = 3;
/// Capability mask of the proximity event: device id, abs X/Y, buttons, tilt X/Y,
/// pressure, rotation (`NSTabletProximity` capability bits).
const CAPS: i64 = 0x0001 | 0x0002 | 0x0004 | 0x0040 | 0x0080 | 0x0100 | 0x0400 | 0x2000;
const DEVICE_ID: i64 = 0x50;

/// Which mouse event the pen state change is (button 0 = left, 1 = right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PenMouse {
	Moved,
	Down(u8),
	Drag(u8),
	Up(u8),
}

/// `held` = the button the tip pressed at touch-down (kept until the lift).
pub(crate) fn pen_mouse(held: Option<u8>, next: &PenState) -> (PenMouse, Option<u8>) {
	match (held, next.in_range && next.contact) {
		(None, true) => {
			let b = next.barrel as u8;
			(PenMouse::Down(b), Some(b))
		}
		(Some(b), true) => (PenMouse::Drag(b), Some(b)),
		(Some(b), false) => (PenMouse::Up(b), None),
		(None, false) => (PenMouse::Moved, None),
	}
}

fn source() -> Option<CGEventSource> {
	CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

/// The bounds (global points) of display `id`, or of the main display when `id` is `None` or
/// no longer active.
pub(crate) fn display_bounds(id: Option<u32>) -> core_graphics::geometry::CGRect {
	if let Some(b) = id.map(|i| CGDisplay::new(i).bounds()).filter(|b| b.size.width > 0.0 && b.size.height > 0.0) {
		return b;
	}
	CGDisplay::main().bounds()
}

fn to_point(display: Option<u32>, x: f32, y: f32) -> CGPoint {
	let b = display_bounds(display);
	CGPoint::new(
		b.origin.x + x.clamp(0.0, 1.0) as f64 * b.size.width,
		b.origin.y + y.clamp(0.0, 1.0) as f64 * b.size.height,
	)
}

fn mouse_type(m: PenMouse) -> (CGEventType, CGMouseButton) {
	match m {
		PenMouse::Moved => (CGEventType::MouseMoved, CGMouseButton::Left),
		PenMouse::Down(1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
		PenMouse::Down(_) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
		PenMouse::Drag(1) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
		PenMouse::Drag(_) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
		PenMouse::Up(1) => (CGEventType::RightMouseUp, CGMouseButton::Right),
		PenMouse::Up(_) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
	}
}

pub(crate) struct MacSink {
	display: Option<u32>,
	pen_held: Option<u8>,
	touch: TouchMouse,
	touch_pressed: bool,
	last_touch: CGPoint,
}

impl MacSink {
	pub(crate) fn new() -> Option<Self> {
		source()?;
		Some(Self { display: None, pen_held: None, touch: TouchMouse::default(), touch_pressed: false, last_touch: CGPoint::new(0.0, 0.0) })
	}

	fn proximity(&self, s: &PenState, enter: bool) {
		let Some(ev) = source().and_then(|src| CGEvent::new(src).ok()) else { return };
		ev.set_type(CGEventType::TabletProximity);
		ev.set_location(to_point(self.display, s.x, s.y));
		ev.set_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_DEVICE_ID, DEVICE_ID);
		ev.set_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_SYSTEM_TABLET_ID, 1);
		ev.set_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_POINTER_ID, 1);
		ev.set_integer_value_field(
			EventField::TABLET_PROXIMITY_EVENT_POINTER_TYPE,
			if s.eraser { POINTER_ERASER } else { POINTER_PEN },
		);
		ev.set_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_CAPABILITY_MASK, CAPS);
		ev.set_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_ENTER_PROXIMITY, enter as i64);
		ev.post(CGEventTapLocation::HID);
	}

	fn pen_event(&self, m: PenMouse, s: &PenState) {
		let (ty, btn) = mouse_type(m);
		let at = to_point(self.display, s.x, s.y);
		let Some(ev) = source().and_then(|src| CGEvent::new_mouse_event(src, ty, at, btn).ok()) else { return };
		// The subtype first: CoreGraphics reads the tablet fields only for a tablet event.
		ev.set_integer_value_field(EventField::MOUSE_EVENT_SUB_TYPE, SUBTYPE_TABLET_POINT);
		let pressure = if s.contact { s.pressure as f64 } else { 0.0 };
		ev.set_double_value_field(EventField::MOUSE_EVENT_PRESSURE, pressure);
		ev.set_double_value_field(EventField::TABLET_EVENT_POINT_PRESSURE, pressure);
		ev.set_double_value_field(EventField::TABLET_EVENT_TILT_X, (s.tilt_x / 90.0) as f64);
		ev.set_double_value_field(EventField::TABLET_EVENT_TILT_Y, (s.tilt_y / 90.0) as f64);
		ev.set_double_value_field(EventField::TABLET_EVENT_ROTATION, s.twist as f64);
		ev.set_integer_value_field(EventField::TABLET_EVENT_POINT_X, at.x.round() as i64);
		ev.set_integer_value_field(EventField::TABLET_EVENT_POINT_Y, at.y.round() as i64);
		ev.set_integer_value_field(EventField::TABLET_EVENT_DEVICE_ID, DEVICE_ID);
		let buttons = (s.contact as i64) | ((s.barrel as i64) << 1);
		ev.set_integer_value_field(EventField::TABLET_EVENT_POINT_BUTTONS, buttons);
		ev.post(CGEventTapLocation::HID);
	}

	fn touch_acts(&mut self, acts: Vec<MouseAct>) {
		for a in acts {
			match a {
				MouseAct::Move { x, y } => {
					self.last_touch = to_point(self.display, x, y);
					let ty = if self.touch_pressed { CGEventType::LeftMouseDragged } else { CGEventType::MouseMoved };
					self.post_mouse(ty, CGMouseButton::Left);
				}
				MouseAct::Button { button, down } => {
					let (ty, btn) = match (button, down) {
						(1, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
						(1, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
						(_, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
						(_, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
					};
					if button == 0 {
						self.touch_pressed = down;
					}
					self.post_mouse(ty, btn);
				}
				MouseAct::Scroll { dx, dy } => {
					let b = display_bounds(self.display);
					// CG: positive wheel = content moves down (scroll toward the top).
					let v = (-dy as f64 * b.size.height).round() as i32;
					let h = (-dx as f64 * b.size.width).round() as i32;
					if v == 0 && h == 0 {
						continue;
					}
					if let Some(ev) =
						source().and_then(|s| CGEvent::new_scroll_event(s, ScrollEventUnit::PIXEL, 2, v, h, 0).ok())
					{
						ev.set_location(self.last_touch);
						ev.post(CGEventTapLocation::HID);
					}
				}
			}
		}
	}

	fn post_mouse(&self, ty: CGEventType, btn: CGMouseButton) {
		if let Some(ev) = source().and_then(|s| CGEvent::new_mouse_event(s, ty, self.last_touch, btn).ok()) {
			ev.post(CGEventTapLocation::HID);
		}
	}
}

impl Sink for MacSink {
	type Geom = Option<u32>;
	fn set_geom(&mut self, g: Option<u32>) {
		self.display = g;
	}
	fn pen(&mut self, prev: &PenState, next: &PenState) {
		let tool_change = prev.in_range && next.in_range && prev.eraser != next.eraser;
		if prev.in_range && (!next.in_range || tool_change) {
			if let Some(b) = self.pen_held.take() {
				self.pen_event(PenMouse::Up(b), &PenState { contact: false, x: next.x, y: next.y, ..*prev });
			}
			self.proximity(prev, false);
		}
		if !next.in_range {
			return;
		}
		if !prev.in_range || tool_change {
			self.proximity(next, true);
		}
		let (m, held) = pen_mouse(self.pen_held, next);
		self.pen_held = held;
		self.pen_event(m, next);
	}
	fn touch(&mut self, now: Instant, frame: &TouchFrame, _changes: &[SlotChange]) {
		let acts = self.touch.update(now, frame);
		self.touch_acts(acts);
	}
	fn tick(&mut self, now: Instant) -> Option<Instant> {
		let (acts, next) = self.touch.tick(now);
		self.touch_acts(acts);
		next
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_tip_presses_the_button_chosen_at_touch_down() {
		let hover = PenState { in_range: true, ..Default::default() };
		let tip = PenState { contact: true, ..hover };
		assert_eq!(pen_mouse(None, &hover), (PenMouse::Moved, None));
		assert_eq!(pen_mouse(None, &PenState { barrel: true, ..tip }), (PenMouse::Down(1), Some(1)));
		// Releasing the barrel mid-stroke keeps the right drag until the lift.
		assert_eq!(pen_mouse(Some(1), &tip), (PenMouse::Drag(1), Some(1)));
		assert_eq!(pen_mouse(Some(1), &hover), (PenMouse::Up(1), None));
		assert_eq!(pen_mouse(None, &tip), (PenMouse::Down(0), Some(0)));
	}
}
