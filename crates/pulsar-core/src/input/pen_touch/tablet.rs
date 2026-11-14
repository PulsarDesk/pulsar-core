//! Client side: pen attributes from the **native** tablet stack, for webviews that report a
//! tablet as a mouse.
//!
//! WKWebView (macOS) gives a Wacom / external tablet `pointerType: "mouse"` — WebKit says
//! "pen" only for the Apple Pencil on iPadOS — and WebKitGTK (Linux) is not known to say
//! "pen" either. The session screen then sends plain mouse events with no pressure, tilt or
//! eraser. This module closes that gap without moving position mapping out of the UI:
//!
//! - a native reader publishes the latest [`TabletSample`] (macOS: a listen-only CGEventTap
//!   for tablet events aimed at this process; Linux: an evdev reader of every tablet node,
//!   read without grabbing it, so the desktop keeps working);
//! - [`TabletAugment`] sits in the session's input path in front of
//!   [`ClientPenTouch`](super::client::ClientPenTouch): while a fresh sample says a pen is in
//!   range, the UI's absolute `PointerMotion` / left `PointerButton` become `InputEvent::Pen`
//!   at the same (already video-mapped) position, carrying the native pressure, tilt, twist,
//!   eraser and barrel state. Real `Pen` events from a webview that does report pens pass
//!   through untouched.
//!
//! Untested on macOS and Linux tablet hardware.

use super::PenState;
use crate::service::InputEvent;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The native pen state the client last saw (not a wire type).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TabletSample {
	pub in_range: bool,
	/// Tip (or eraser) touching.
	pub contact: bool,
	/// 0..1.
	pub pressure: f32,
	/// Degrees, -90..90.
	pub tilt_x: f32,
	pub tilt_y: f32,
	/// Degrees, 0..360.
	pub twist: f32,
	pub eraser: bool,
	pub barrel: bool,
}

/// A mouse event is the pen's only when a sample is at most this old (the native event
/// precedes the webview's; an older sample means a real mouse moved).
pub const FRESH: Duration = Duration::from_millis(500);

static LATEST: Mutex<Option<(TabletSample, Instant)>> = Mutex::new(None);

/// Record a native sample (reader threads).
pub fn publish(s: TabletSample) {
	*LATEST.lock().unwrap() = Some((s, Instant::now()));
}

/// The latest native sample and when it arrived.
pub fn latest() -> Option<(TabletSample, Instant)> {
	*LATEST.lock().unwrap()
}

/// Start the platform's native tablet reader once per process (no-op where the UI already
/// gets real pen events: Windows, whose renderer forwards WM_POINTER pens).
pub fn start_native() {
	static ONCE: std::sync::Once = std::sync::Once::new();
	ONCE.call_once(|| {
		#[cfg(target_os = "macos")]
		mac::start();
		#[cfg(target_os = "linux")]
		lnx::start();
	});
}

/// The per-session converter (see the module docs). Pure: the sample is passed in.
#[derive(Default)]
pub struct TabletAugment {
	/// Last position the UI reported (0..1 of the video).
	pos: Option<(f32, f32)>,
	/// A pen is being sent for the mouse right now.
	pen_on: bool,
	/// The UI's left button is down while `pen_on` (the tip, as the webview saw it).
	tip: bool,
}

fn fresh(now: Instant, s: Option<(TabletSample, Instant)>) -> Option<TabletSample> {
	let (s, at) = s?;
	(s.in_range && now.saturating_duration_since(at) <= FRESH).then_some(s)
}

impl TabletAugment {
	fn pen(&self, s: &TabletSample) -> Option<InputEvent> {
		let (x, y) = self.pos?;
		let contact = self.tip || s.contact;
		Some(InputEvent::Pen(PenState {
			x,
			y,
			pressure: if contact { if s.pressure > 0.0 { s.pressure } else { 0.5 } } else { 0.0 },
			tilt_x: s.tilt_x,
			tilt_y: s.tilt_y,
			twist: s.twist,
			in_range: true,
			contact,
			eraser: s.eraser,
			barrel: s.barrel,
		}))
	}

	fn lift(&mut self) -> Option<InputEvent> {
		if !std::mem::take(&mut self.pen_on) {
			return None;
		}
		self.tip = false;
		let (x, y) = self.pos.unwrap_or((0.5, 0.5));
		Some(InputEvent::Pen(PenState { x, y, ..Default::default() }))
	}

	/// One event from the UI → what to send instead.
	pub fn on_event(&mut self, now: Instant, ev: InputEvent, sample: Option<(TabletSample, Instant)>) -> Vec<InputEvent> {
		let s = fresh(now, sample);
		match ev {
			InputEvent::PointerMotion { x, y } => {
				self.pos = Some((x as f32, y as f32));
				if let Some(s) = s {
					self.pen_on = true;
					return self.pen(&s).into_iter().collect();
				}
			}
			InputEvent::PointerButton { button: 0, down } if self.pen_on || s.is_some() => {
				if let Some(s) = s.or_else(|| sample.map(|(s, _)| s).filter(|s| s.in_range)) {
					if self.pos.is_some() {
						self.pen_on = true;
						self.tip = down;
						return self.pen(&s).into_iter().collect();
					}
				}
			}
			// The webview reports this pen itself: nothing to add.
			InputEvent::Pen(_) => {
				self.pen_on = false;
				self.tip = false;
				return vec![ev];
			}
			_ => return vec![ev],
		}
		// Not the pen (any more): lift what was sent, then the mouse event as it was.
		let mut out: Vec<InputEvent> = self.lift().into_iter().collect();
		out.push(ev);
		out
	}

	/// The pen left range with no UI event after it: lift. Call with the session's timer.
	pub fn tick(&mut self, sample: Option<(TabletSample, Instant)>) -> Vec<InputEvent> {
		if self.pen_on && !sample.is_some_and(|(s, _)| s.in_range) {
			return self.lift().into_iter().collect();
		}
		Vec::new()
	}
}

/// Linux evdev codes the tablet reader folds (raw values, so the fold is testable anywhere).
pub mod evcodes {
	pub const EV_SYN: u16 = 0x00;
	pub const EV_KEY: u16 = 0x01;
	pub const EV_ABS: u16 = 0x03;
	pub const SYN_REPORT: u16 = 0;
	pub const BTN_TOOL_PEN: u16 = 0x140;
	pub const BTN_TOOL_RUBBER: u16 = 0x141;
	pub const BTN_TOUCH: u16 = 0x14a;
	pub const BTN_STYLUS: u16 = 0x14b;
	pub const BTN_STYLUS2: u16 = 0x14c;
	pub const ABS_PRESSURE: u16 = 0x18;
	pub const ABS_TILT_X: u16 = 0x1a;
	pub const ABS_TILT_Y: u16 = 0x1b;
}

/// The range of one absolute axis (`input_absinfo`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxisRange {
	pub min: i32,
	pub max: i32,
	/// Units per radian for tilt (0 = unknown).
	pub resolution: i32,
}

/// Folds a tablet node's evdev stream into [`TabletSample`]s (one per `SYN_REPORT`).
#[derive(Default)]
pub struct EvdevPen {
	pub pressure: AxisRange,
	pub tilt_x: AxisRange,
	pub tilt_y: AxisRange,
	pen: bool,
	rubber: bool,
	cur: TabletSample,
}

/// Tilt in degrees: by the axis resolution (units per radian) when known, else the range
/// taken as -64..63 degrees (the usual Wacom report).
pub fn tilt_deg(v: i32, r: AxisRange) -> f32 {
	let d = if r.resolution > 0 {
		(v as f32 / r.resolution as f32).to_degrees()
	} else if r.max > r.min {
		let mid = (r.max as f32 + r.min as f32) / 2.0;
		(v as f32 - mid) / ((r.max - r.min) as f32 / 2.0) * 64.0
	} else {
		0.0
	};
	d.clamp(-90.0, 90.0)
}

impl EvdevPen {
	/// One evdev event; `Some` on a `SYN_REPORT` that completes a sample.
	pub fn feed(&mut self, ty: u16, code: u16, value: i32) -> Option<TabletSample> {
		use evcodes::*;
		match (ty, code) {
			(EV_KEY, BTN_TOOL_PEN) => self.pen = value != 0,
			(EV_KEY, BTN_TOOL_RUBBER) => self.rubber = value != 0,
			(EV_KEY, BTN_TOUCH) => self.cur.contact = value != 0,
			(EV_KEY, BTN_STYLUS) | (EV_KEY, BTN_STYLUS2) => self.cur.barrel = value != 0,
			(EV_ABS, ABS_PRESSURE) => {
				let r = self.pressure;
				self.cur.pressure =
					if r.max > r.min { ((value - r.min) as f32 / (r.max - r.min) as f32).clamp(0.0, 1.0) } else { 0.0 };
			}
			(EV_ABS, ABS_TILT_X) => self.cur.tilt_x = tilt_deg(value, self.tilt_x),
			(EV_ABS, ABS_TILT_Y) => self.cur.tilt_y = tilt_deg(value, self.tilt_y),
			(EV_SYN, SYN_REPORT) => {
				self.cur.in_range = self.pen || self.rubber;
				self.cur.eraser = self.rubber;
				if !self.cur.in_range {
					self.cur.contact = false;
					self.cur.pressure = 0.0;
				}
				return Some(self.cur);
			}
			_ => {}
		}
		None
	}
}

#[cfg(target_os = "linux")]
mod lnx {
	//! Every evdev node with `BTN_TOOL_PEN` + `ABS_PRESSURE` (a tablet or a pen display),
	//! read without `EVIOCGRAB`; rescanned for hot-plugged tablets. Needs read access to
	//! `/dev/input` (the `input` group), like the native game-mode capture.
	use super::{publish, AxisRange, EvdevPen};
	use evdev::{AbsoluteAxisCode, Device, KeyCode};
	use std::collections::HashSet;
	use std::path::PathBuf;
	use std::sync::{Arc, Mutex};

	/// Pulsar's own uinput tablet (a host on the same machine) — never read it back.
	const OWN_VID: u16 = 0x5055;

	fn is_pen(d: &Device) -> bool {
		d.input_id().vendor() != OWN_VID
			&& d.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_TOOL_PEN))
			&& d.supported_absolute_axes().is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_PRESSURE))
	}

	fn range(d: &Device, axis: AbsoluteAxisCode) -> AxisRange {
		d.get_absinfo()
			.ok()
			.and_then(|mut it| it.find(|(a, _)| *a == axis))
			.map(|(_, i)| AxisRange { min: i.minimum(), max: i.maximum(), resolution: i.resolution() })
			.unwrap_or_default()
	}

	fn read(path: PathBuf, mut dev: Device, open: Arc<Mutex<HashSet<PathBuf>>>) {
		let mut fold = EvdevPen {
			pressure: range(&dev, AbsoluteAxisCode::ABS_PRESSURE),
			tilt_x: range(&dev, AbsoluteAxisCode::ABS_TILT_X),
			tilt_y: range(&dev, AbsoluteAxisCode::ABS_TILT_Y),
			..Default::default()
		};
		tracing::info!(device = %path.display(), name = dev.name().unwrap_or(""), "client tablet reader started");
		'outer: loop {
			match dev.fetch_events() {
				Ok(events) => {
					for e in events {
						if let Some(s) = fold.feed(e.event_type().0, e.code(), e.value()) {
							publish(s);
						}
					}
				}
				Err(_) => break 'outer, // unplugged
			}
		}
		publish(Default::default());
		open.lock().unwrap().remove(&path);
	}

	pub(super) fn start() {
		let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
		let _ = std::thread::Builder::new().name("pulsar-tablet-scan".into()).spawn(move || {
			let mut warned = false;
			loop {
				match std::fs::read_dir("/dev/input") {
					Ok(dir) => {
						for p in dir.flatten().map(|e| e.path()) {
							if !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"))
								|| open.lock().unwrap().contains(&p)
							{
								continue;
							}
							let Ok(dev) = Device::open(&p) else { continue };
							if !is_pen(&dev) {
								continue;
							}
							open.lock().unwrap().insert(p.clone());
							let o = open.clone();
							let _ = std::thread::Builder::new()
								.name("pulsar-tablet".into())
								.spawn(move || read(p, dev, o));
						}
					}
					Err(e) if !warned => {
						warned = true;
						tracing::info!(err = %e, "client tablet reader: /dev/input unreadable (no pen attributes)");
					}
					Err(_) => {}
				}
				std::thread::sleep(std::time::Duration::from_secs(5));
			}
		});
	}
}

#[cfg(target_os = "macos")]
mod mac {
	//! A listen-only CGEventTap at the annotated-session location for tablet proximity and
	//! the tablet-point mouse events aimed at this process (the app's windows, i.e. the
	//! WKWebView). Listen-only mouse / tablet taps need no Accessibility grant; if macOS
	//! refuses the tap anyway, pens stay plain mice (logged once).
	use super::{publish, TabletSample};
	use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
	use core_graphics::event::{
		CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, CallbackResult,
		EventField,
	};
	use std::sync::Mutex;

	/// `kCGEventMouseSubtypeTabletPoint` / `…TabletProximity`.
	const SUBTYPE_POINT: i64 = 1;
	const SUBTYPE_PROXIMITY: i64 = 2;
	/// `NSPointingDeviceType`: eraser / cursor (puck — a mouse, not a pen).
	const ERASER: i64 = 3;
	const CURSOR: i64 = 2;

	static STATE: Mutex<TabletSample> =
		Mutex::new(TabletSample { in_range: false, contact: false, pressure: 0.0, tilt_x: 0.0, tilt_y: 0.0, twist: 0.0, eraser: false, barrel: false });

	fn on_event(ty: CGEventType, ev: &CGEvent, pid: i64) {
		let sub = ev.get_integer_value_field(EventField::MOUSE_EVENT_SUB_TYPE);
		let proximity = matches!(ty, CGEventType::TabletProximity) || sub == SUBTYPE_PROXIMITY;
		let mut s = STATE.lock().unwrap();
		if proximity {
			let enter = ev.get_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_ENTER_PROXIMITY) != 0;
			let kind = ev.get_integer_value_field(EventField::TABLET_PROXIMITY_EVENT_POINTER_TYPE);
			s.in_range = enter && kind != CURSOR;
			s.eraser = kind == ERASER;
			if !s.in_range {
				s.contact = false;
				s.pressure = 0.0;
			}
			publish(*s);
			return;
		}
		let point = matches!(ty, CGEventType::TabletPointer) || sub == SUBTYPE_POINT;
		if !point || ev.get_integer_value_field(EventField::EVENT_TARGET_UNIX_PROCESS_ID) != pid {
			return;
		}
		let buttons = ev.get_integer_value_field(EventField::TABLET_EVENT_POINT_BUTTONS);
		s.in_range = true;
		s.contact = buttons & 1 != 0;
		s.barrel = buttons & 2 != 0;
		s.pressure = (ev.get_double_value_field(EventField::TABLET_EVENT_POINT_PRESSURE) as f32).clamp(0.0, 1.0);
		// CoreGraphics tilt is -1..1 (full deflection), rotation in degrees.
		s.tilt_x = (ev.get_double_value_field(EventField::TABLET_EVENT_TILT_X) as f32 * 90.0).clamp(-90.0, 90.0);
		s.tilt_y = (ev.get_double_value_field(EventField::TABLET_EVENT_TILT_Y) as f32 * 90.0).clamp(-90.0, 90.0);
		s.twist = (ev.get_double_value_field(EventField::TABLET_EVENT_ROTATION) as f32).rem_euclid(360.0);
		publish(*s);
	}

	pub(super) fn start() {
		let _ = std::thread::Builder::new().name("pulsar-tablet-tap".into()).spawn(|| {
			let pid = std::process::id() as i64;
			let types = vec![
				CGEventType::TabletPointer,
				CGEventType::TabletProximity,
				CGEventType::MouseMoved,
				CGEventType::LeftMouseDown,
				CGEventType::LeftMouseUp,
				CGEventType::LeftMouseDragged,
				CGEventType::RightMouseDown,
				CGEventType::RightMouseUp,
				CGEventType::RightMouseDragged,
			];
			let tap = CGEventTap::new(
				CGEventTapLocation::AnnotatedSession,
				CGEventTapPlacement::TailAppendEventTap,
				CGEventTapOptions::ListenOnly,
				types,
				move |_proxy, ty, ev| {
					on_event(ty, ev, pid);
					CallbackResult::Keep
				},
			);
			let Ok(tap) = tap else {
				tracing::info!("client tablet tap refused by macOS — pens are sent as a mouse");
				return;
			};
			let Ok(src) = tap.mach_port().create_runloop_source(0) else { return };
			CFRunLoop::get_current().add_source(&src, unsafe { kCFRunLoopCommonModes });
			tap.enable();
			tracing::info!("client tablet tap started");
			CFRunLoop::run_current();
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pen_sample() -> TabletSample {
		TabletSample { in_range: true, pressure: 0.7, tilt_x: 20.0, tilt_y: -10.0, twist: 45.0, ..Default::default() }
	}

	#[test]
	fn a_mouse_with_a_fresh_tablet_sample_becomes_the_pen() {
		let t = Instant::now();
		let mut a = TabletAugment::default();
		let s = Some((pen_sample(), t));
		let hover = a.on_event(t, InputEvent::PointerMotion { x: 0.25, y: 0.5 }, s);
		let [InputEvent::Pen(p)] = hover[..] else { panic!("{hover:?}") };
		assert!(p.in_range && !p.contact && p.pressure == 0.0 && p.tilt_x == 20.0 && p.twist == 45.0);
		assert_eq!((p.x, p.y), (0.25, 0.5));
		let down = a.on_event(t, InputEvent::PointerButton { button: 0, down: true }, s);
		let [InputEvent::Pen(p)] = down[..] else { panic!() };
		assert!(p.contact && p.pressure == 0.7);
		let up = a.on_event(t, InputEvent::PointerButton { button: 0, down: false }, s);
		let [InputEvent::Pen(p)] = up[..] else { panic!() };
		assert!(p.in_range && !p.contact);
		// The right button (a side switch mapped to right click) stays a mouse button.
		let r = InputEvent::PointerButton { button: 1, down: true };
		assert_eq!(a.on_event(t, r, s), vec![r]);
	}

	#[test]
	fn a_stale_sample_means_the_mouse_moved_and_the_pen_is_lifted() {
		let t = Instant::now();
		let mut a = TabletAugment::default();
		a.on_event(t, InputEvent::PointerMotion { x: 0.1, y: 0.1 }, Some((pen_sample(), t)));
		let later = t + FRESH + Duration::from_millis(1);
		let m = InputEvent::PointerMotion { x: 0.2, y: 0.2 };
		let out = a.on_event(later, m, Some((pen_sample(), t)));
		assert_eq!(out.len(), 2);
		assert!(matches!(out[0], InputEvent::Pen(p) if !p.in_range));
		assert_eq!(out[1], m);
		// No pen at all: everything passes untouched.
		assert_eq!(a.on_event(later, m, None), vec![m]);
	}

	#[test]
	fn leaving_range_lifts_on_the_timer_and_real_pens_pass() {
		let t = Instant::now();
		let mut a = TabletAugment::default();
		a.on_event(t, InputEvent::PointerMotion { x: 0.1, y: 0.1 }, Some((pen_sample(), t)));
		assert!(a.tick(Some((pen_sample(), t))).is_empty());
		let out = a.tick(Some((TabletSample::default(), t)));
		assert!(matches!(out[..], [InputEvent::Pen(p)] if !p.in_range));
		assert!(a.tick(None).is_empty());
		let real = InputEvent::Pen(PenState { in_range: true, ..Default::default() });
		assert_eq!(a.on_event(t, real, Some((pen_sample(), t))), vec![real]);
	}

	#[test]
	fn evdev_fold_tracks_tool_contact_and_axes() {
		use evcodes::*;
		let mut f = EvdevPen {
			pressure: AxisRange { min: 0, max: 2047, resolution: 0 },
			tilt_x: AxisRange { min: -64, max: 63, resolution: 57 },
			tilt_y: AxisRange { min: 0, max: 127, resolution: 0 },
			..Default::default()
		};
		assert!(f.feed(EV_KEY, BTN_TOOL_RUBBER, 1).is_none());
		f.feed(EV_KEY, BTN_TOUCH, 1);
		f.feed(EV_ABS, ABS_PRESSURE, 2047);
		f.feed(EV_ABS, ABS_TILT_X, 57);
		f.feed(EV_ABS, ABS_TILT_Y, 127);
		let s = f.feed(EV_SYN, SYN_REPORT, 0).unwrap();
		assert!(s.in_range && s.eraser && s.contact && s.pressure == 1.0);
		assert!((s.tilt_x - 57.3).abs() < 0.1, "{}", s.tilt_x);
		assert!((s.tilt_y - 64.0).abs() < 1.0, "{}", s.tilt_y);
		f.feed(EV_KEY, BTN_TOOL_RUBBER, 0);
		let s = f.feed(EV_SYN, SYN_REPORT, 0).unwrap();
		assert!(!s.in_range && !s.contact && s.pressure == 0.0);
	}
}
