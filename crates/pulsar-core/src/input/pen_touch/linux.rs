//! Linux pen + touch injection through **uinput**, created lazily on first use:
//!
//! - "Pulsar Virtual Pen": a direct (screen-mapped) tablet — `BTN_TOOL_PEN` /
//!   `BTN_TOOL_RUBBER` proximity, `BTN_TOUCH` tip contact, `BTN_STYLUS` barrel button,
//!   `ABS_X/Y`, `ABS_PRESSURE` (0..4096), `ABS_TILT_X/Y` (degrees). libinput reports it as a
//!   tablet tool, so GIMP / Krita / Xournal++ get pressure and tilt on X11 and Wayland.
//! - "Pulsar Virtual Touchscreen": a direct multitouch device, protocol type B
//!   (`ABS_MT_SLOT` / `ABS_MT_TRACKING_ID` / `ABS_MT_POSITION_X/Y`, [`MAX_TOUCH`] slots) plus
//!   the single-touch `ABS_X/Y` + `BTN_TOUCH` emulation and `BTN_TOOL_*TAP` finger counts.
//!
//! Both are mapped over the bounding box of all outputs by the compositor (like the absolute
//! pointer), so positions go through the same streamed-monitor region.
//!
//! Untested on real pen / touch hardware.

use super::{Contact, PenState, Sink, SlotChange, TouchFrame, MAX_TOUCH};
use crate::capture::PointerRegion;
use evdev::{
	uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode as Abs, AttributeSet, BusType, EventType, InputEvent,
	InputId, KeyCode, PropType, UinputAbsSetup,
};
use std::time::Instant;

const POS_MAX: i32 = 32767;
const PRESSURE_MAX: i32 = 4096;
/// Device units per mm (so libinput / toolkits can compute a physical size; a 32767-unit
/// axis at 100 units/mm reads as a ~33 cm surface).
const RES: i32 = 100;

fn ev(ty: EventType, code: u16, value: i32) -> InputEvent {
	InputEvent::new(ty.0, code, value)
}
fn key(k: KeyCode, down: bool) -> InputEvent {
	ev(EventType::KEY, k.code(), down as i32)
}
fn abs(a: Abs, v: i32) -> InputEvent {
	ev(EventType::ABSOLUTE, a.0, v)
}

/// Normalized (streamed-monitor) → device units, through the monitor's region of the layout.
fn to_dev(region: Option<PointerRegion>, x: f32, y: f32) -> (i32, i32) {
	let (mut x, mut y) = (x.clamp(0.0, 1.0) as f64, y.clamp(0.0, 1.0) as f64);
	if let Some(r) = region {
		x = r.x + x * r.w;
		y = r.y + y * r.h;
	}
	(
		(x.clamp(0.0, 1.0) * POS_MAX as f64).round() as i32,
		(y.clamp(0.0, 1.0) * POS_MAX as f64).round() as i32,
	)
}

fn tool(s: &PenState) -> KeyCode {
	if s.eraser {
		KeyCode::BTN_TOOL_RUBBER
	} else {
		KeyCode::BTN_TOOL_PEN
	}
}

/// One pen report (several when the tool changes: the old tool leaves in its own report).
pub(crate) fn pen_reports(region: Option<PointerRegion>, prev: &PenState, next: &PenState) -> Vec<Vec<InputEvent>> {
	let mut out = Vec::new();
	let leaving = prev.in_range && (!next.in_range || prev.eraser != next.eraser);
	if leaving {
		let mut r = Vec::new();
		if prev.contact {
			r.push(abs(Abs::ABS_PRESSURE, 0));
			r.push(key(KeyCode::BTN_TOUCH, false));
		}
		if prev.barrel {
			r.push(key(KeyCode::BTN_STYLUS, false));
		}
		r.push(key(tool(prev), false));
		out.push(r);
	}
	if !next.in_range {
		return out;
	}
	let entering = !prev.in_range || leaving;
	let was = if entering { PenState::default() } else { *prev };
	let (x, y) = to_dev(region, next.x, next.y);
	let mut r = vec![
		abs(Abs::ABS_X, x),
		abs(Abs::ABS_Y, y),
		abs(Abs::ABS_TILT_X, next.tilt_x.round() as i32),
		abs(Abs::ABS_TILT_Y, next.tilt_y.round() as i32),
		abs(Abs::ABS_PRESSURE, (next.pressure * PRESSURE_MAX as f32).round() as i32),
	];
	if entering {
		r.push(key(tool(next), true));
	}
	if was.barrel != next.barrel {
		r.push(key(KeyCode::BTN_STYLUS, next.barrel));
	}
	if was.contact != next.contact {
		r.push(key(KeyCode::BTN_TOUCH, next.contact));
	}
	out.push(r);
	out
}

/// One multitouch frame (protocol B) for the slot changes; `count` = fingers after it.
pub(crate) fn touch_report(
	region: Option<PointerRegion>,
	frame: &TouchFrame,
	changes: &[SlotChange],
	next_tracking: &mut i32,
) -> Vec<InputEvent> {
	let mut r = Vec::new();
	for c in changes {
		r.push(abs(Abs::ABS_MT_SLOT, c.slot as i32));
		match c.phase {
			Contact::Up => r.push(abs(Abs::ABS_MT_TRACKING_ID, -1)),
			Contact::Down | Contact::Move => {
				if c.phase == Contact::Down {
					r.push(abs(Abs::ABS_MT_TRACKING_ID, *next_tracking));
					*next_tracking = (*next_tracking + 1) & 0xffff;
				}
				let (x, y) = to_dev(region, c.pt.x, c.pt.y);
				r.push(abs(Abs::ABS_MT_POSITION_X, x));
				r.push(abs(Abs::ABS_MT_POSITION_Y, y));
			}
		}
	}
	// Single-touch emulation: the oldest finger, the touch bit and the finger count.
	if let Some(p) = frame.points().first() {
		let (x, y) = to_dev(region, p.x, p.y);
		r.push(abs(Abs::ABS_X, x));
		r.push(abs(Abs::ABS_Y, y));
	}
	let n = frame.len();
	r.push(key(KeyCode::BTN_TOUCH, n > 0));
	for (i, k) in [
		KeyCode::BTN_TOOL_FINGER,
		KeyCode::BTN_TOOL_DOUBLETAP,
		KeyCode::BTN_TOOL_TRIPLETAP,
		KeyCode::BTN_TOOL_QUADTAP,
		KeyCode::BTN_TOOL_QUINTTAP,
	]
	.into_iter()
	.enumerate()
	{
		r.push(key(k, n == i + 1 || (i == 4 && n > 5)));
	}
	r
}

fn new_pen() -> std::io::Result<VirtualDevice> {
	let mut keys = AttributeSet::<KeyCode>::new();
	for k in [KeyCode::BTN_TOOL_PEN, KeyCode::BTN_TOOL_RUBBER, KeyCode::BTN_TOUCH, KeyCode::BTN_STYLUS] {
		keys.insert(k);
	}
	let mut props = AttributeSet::<PropType>::new();
	props.insert(PropType::DIRECT);
	let pos = |a| UinputAbsSetup::new(a, AbsInfo::new(0, 0, POS_MAX, 0, 0, RES));
	// Tilt resolution is in units per radian: whole degrees → 57.
	let tilt = |a| UinputAbsSetup::new(a, AbsInfo::new(0, -90, 90, 0, 0, 57));
	VirtualDevice::builder()?
		.name("Pulsar Virtual Pen")
		.input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5055, 0x0001, 1))
		.with_properties(&props)?
		.with_keys(&keys)?
		.with_absolute_axis(&pos(Abs::ABS_X))?
		.with_absolute_axis(&pos(Abs::ABS_Y))?
		.with_absolute_axis(&UinputAbsSetup::new(Abs::ABS_PRESSURE, AbsInfo::new(0, 0, PRESSURE_MAX, 0, 0, 0)))?
		.with_absolute_axis(&tilt(Abs::ABS_TILT_X))?
		.with_absolute_axis(&tilt(Abs::ABS_TILT_Y))?
		.build()
}

fn new_touch() -> std::io::Result<VirtualDevice> {
	let mut keys = AttributeSet::<KeyCode>::new();
	for k in [
		KeyCode::BTN_TOUCH,
		KeyCode::BTN_TOOL_FINGER,
		KeyCode::BTN_TOOL_DOUBLETAP,
		KeyCode::BTN_TOOL_TRIPLETAP,
		KeyCode::BTN_TOOL_QUADTAP,
		KeyCode::BTN_TOOL_QUINTTAP,
	] {
		keys.insert(k);
	}
	let mut props = AttributeSet::<PropType>::new();
	props.insert(PropType::DIRECT);
	let pos = |a| UinputAbsSetup::new(a, AbsInfo::new(0, 0, POS_MAX, 0, 0, RES));
	VirtualDevice::builder()?
		.name("Pulsar Virtual Touchscreen")
		.input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5055, 0x0002, 1))
		.with_properties(&props)?
		.with_keys(&keys)?
		.with_absolute_axis(&pos(Abs::ABS_X))?
		.with_absolute_axis(&pos(Abs::ABS_Y))?
		.with_absolute_axis(&UinputAbsSetup::new(Abs::ABS_MT_SLOT, AbsInfo::new(0, 0, MAX_TOUCH as i32 - 1, 0, 0, 0)))?
		.with_absolute_axis(&UinputAbsSetup::new(Abs::ABS_MT_TRACKING_ID, AbsInfo::new(0, 0, 0xffff, 0, 0, 0)))?
		.with_absolute_axis(&pos(Abs::ABS_MT_POSITION_X))?
		.with_absolute_axis(&pos(Abs::ABS_MT_POSITION_Y))?
		.build()
}

pub(crate) struct UinputSink {
	pen: Option<VirtualDevice>,
	touch: Option<VirtualDevice>,
	region: Option<PointerRegion>,
	tracking: i32,
	failed: (bool, bool),
}

impl UinputSink {
	pub(crate) fn new() -> Option<Self> {
		Some(Self { pen: None, touch: None, region: None, tracking: 0, failed: (false, false) })
	}

	fn pen_dev(&mut self) -> Option<&mut VirtualDevice> {
		if self.pen.is_none() && !self.failed.0 {
			match new_pen() {
				Ok(d) => self.pen = Some(d),
				Err(e) => {
					self.failed.0 = true;
					tracing::warn!(err = %e, "uinput pen device unavailable — pen not injected");
				}
			}
		}
		self.pen.as_mut()
	}

	fn touch_dev(&mut self) -> Option<&mut VirtualDevice> {
		if self.touch.is_none() && !self.failed.1 {
			match new_touch() {
				Ok(d) => self.touch = Some(d),
				Err(e) => {
					self.failed.1 = true;
					tracing::warn!(err = %e, "uinput touchscreen unavailable — touch not injected");
				}
			}
		}
		self.touch.as_mut()
	}
}

impl Sink for UinputSink {
	type Geom = Option<PointerRegion>;
	fn set_geom(&mut self, g: Self::Geom) {
		self.region = g;
	}
	fn pen(&mut self, prev: &PenState, next: &PenState) {
		let reports = pen_reports(self.region, prev, next);
		if let Some(d) = self.pen_dev() {
			for r in reports {
				let _ = d.emit(&r);
			}
		}
	}
	fn touch(&mut self, _now: Instant, frame: &TouchFrame, changes: &[SlotChange]) {
		let mut tracking = self.tracking;
		let r = touch_report(self.region, frame, changes, &mut tracking);
		self.tracking = tracking;
		if let Some(d) = self.touch_dev() {
			let _ = d.emit(&r);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::super::{TouchPoint, TouchTracker};
	use super::*;

	fn has(r: &[InputEvent], ty: EventType, code: u16, v: i32) -> bool {
		r.iter().any(|e| e.event_type() == ty && e.code() == code && e.value() == v)
	}

	#[test]
	fn pen_enters_touches_and_leaves() {
		let hover = PenState { x: 0.5, y: 0.5, in_range: true, ..Default::default() };
		let tip = PenState { contact: true, pressure: 0.5, ..hover };
		let r = pen_reports(None, &PenState::default(), &tip);
		assert_eq!(r.len(), 1);
		assert!(has(&r[0], EventType::KEY, KeyCode::BTN_TOOL_PEN.code(), 1));
		assert!(has(&r[0], EventType::KEY, KeyCode::BTN_TOUCH.code(), 1));
		assert!(has(&r[0], EventType::ABSOLUTE, Abs::ABS_PRESSURE.0, 2048));
		assert!(has(&r[0], EventType::ABSOLUTE, Abs::ABS_X.0, 16384));
		let r = pen_reports(None, &tip, &PenState::default());
		assert_eq!(r.len(), 1);
		assert!(has(&r[0], EventType::KEY, KeyCode::BTN_TOUCH.code(), 0));
		assert!(has(&r[0], EventType::KEY, KeyCode::BTN_TOOL_PEN.code(), 0));
		// Flipping to the eraser: the pen leaves, the rubber enters.
		let r = pen_reports(None, &hover, &PenState { eraser: true, ..hover });
		assert_eq!(r.len(), 2);
		assert!(has(&r[1], EventType::KEY, KeyCode::BTN_TOOL_RUBBER.code(), 1));
	}

	#[test]
	fn touch_frames_follow_protocol_b() {
		let mut t = TouchTracker::default();
		let mut id = 7;
		let f = TouchFrame::new(&[TouchPoint { id: 50, x: 0.0, y: 1.0, pressure: 0.0 }]);
		let r = touch_report(None, &f, &t.update(&f), &mut id);
		assert!(has(&r, EventType::ABSOLUTE, Abs::ABS_MT_TRACKING_ID.0, 7));
		assert!(has(&r, EventType::ABSOLUTE, Abs::ABS_MT_POSITION_Y.0, POS_MAX));
		assert!(has(&r, EventType::KEY, KeyCode::BTN_TOOL_FINGER.code(), 1));
		assert_eq!(id, 8);
		let empty = TouchFrame::default();
		let r = touch_report(None, &empty, &t.update(&empty), &mut id);
		assert!(has(&r, EventType::ABSOLUTE, Abs::ABS_MT_TRACKING_ID.0, -1));
		assert!(has(&r, EventType::KEY, KeyCode::BTN_TOUCH.code(), 0));
	}

	#[test]
	fn positions_follow_the_streamed_monitor_region() {
		let r = PointerRegion { x: 0.5, y: 0.0, w: 0.5, h: 1.0 };
		assert_eq!(to_dev(Some(r), 0.0, 0.0), (16384, 0));
		assert_eq!(to_dev(Some(r), 1.0, 1.0), (POS_MAX, POS_MAX));
	}
}
