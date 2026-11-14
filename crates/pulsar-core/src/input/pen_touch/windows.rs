//! Windows pen + touch injection through **synthetic pointer devices**
//! (`CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`, Windows 10 1809+): a
//! `PT_PEN` device (one contact: pressure, tilt, rotation, eraser, barrel, hover) and a
//! `PT_TOUCH` device (up to [`MAX_TOUCH`] contacts). Apps receive real `WM_POINTER*` pen /
//! touch input — Windows Ink, palm-free drawing, pressure, touch gestures — like Sunshine does.
//!
//! The functions are resolved at run time, so the app still starts on an older Windows (pen
//! and touch then log once and do nothing). Injected pointers that are not refreshed get
//! canceled by the system, hence [`REFRESH`]. The worker thread is per-monitor-v2 DPI aware,
//! so `ptPixelLocation` is in the same physical virtual-desktop pixels as the capture's
//! `MonitorRect`.
//!
//! Untested on real pen / touch hardware.

use super::{Contact, PenState, Sink, SlotChange, TouchFrame, TouchPoint, MAX_TOUCH};
use crate::input::MonitorRect;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::Controls::{
	HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT, POINTER_FEEDBACK_MODE, POINTER_TYPE_INFO,
	POINTER_TYPE_INFO_0,
};
use windows_sys::Win32::UI::Input::Pointer::{
	POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
	POINTER_FLAG_SECONDBUTTON, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_PEN_INFO,
	POINTER_TOUCH_INFO,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
	GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE,
	PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH,
	SM_CXSCREEN, SM_CYSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
};

/// Re-inject the unchanged state this often while anything is in range / down.
const REFRESH: Duration = Duration::from_millis(50);

type FnCreate = unsafe extern "system" fn(POINTER_INPUT_TYPE, u32, POINTER_FEEDBACK_MODE) -> HSYNTHETICPOINTERDEVICE;
type FnInject = unsafe extern "system" fn(HSYNTHETICPOINTERDEVICE, *const POINTER_TYPE_INFO, u32) -> i32;
type FnDestroy = unsafe extern "system" fn(HSYNTHETICPOINTERDEVICE);
type FnDpi = unsafe extern "system" fn(isize) -> isize;

struct Api {
	create: FnCreate,
	inject: FnInject,
	destroy: FnDestroy,
}

fn user32_proc(name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
	use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
	let dll: Vec<u16> = "user32.dll\0".encode_utf16().collect();
	// SAFETY: NUL-terminated names; user32 is loaded in every GUI process (LoadLibraryW only
	// bumps its refcount if not).
	unsafe {
		let mut h = GetModuleHandleW(dll.as_ptr());
		if h.is_null() {
			h = LoadLibraryW(dll.as_ptr());
		}
		if h.is_null() {
			return None;
		}
		GetProcAddress(h, name.as_ptr())
	}
}

impl Api {
	fn load() -> Option<Self> {
		let create = user32_proc(b"CreateSyntheticPointerDevice\0")?;
		let inject = user32_proc(b"InjectSyntheticPointerInput\0")?;
		let destroy = user32_proc(b"DestroySyntheticPointerDevice\0")?;
		// SAFETY: the documented signatures of these user32 exports (see the type aliases).
		unsafe {
			Some(Self {
				create: std::mem::transmute::<_, FnCreate>(create),
				inject: std::mem::transmute::<_, FnInject>(inject),
				destroy: std::mem::transmute::<_, FnDestroy>(destroy),
			})
		}
	}
}

/// The worker thread's DPI awareness → per-monitor v2 (physical pixels), best effort.
fn per_monitor_dpi() {
	if let Some(f) = user32_proc(b"SetThreadDpiAwarenessContext\0") {
		// SAFETY: SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT) -> DPI_AWARENESS_CONTEXT;
		// -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2.
		unsafe { std::mem::transmute::<_, FnDpi>(f)(-4) };
	}
}

/// Normalized (0..1 of the streamed monitor) → virtual-desktop pixel. `None` geometry = the
/// primary display (`primary` = its size), like `DesktopInput::pointer`.
pub(crate) fn to_px(g: Option<MonitorRect>, primary: (i32, i32), x: f32, y: f32) -> POINT {
	let (left, top, w, h) = match g {
		Some(m) if m.mon_width > 0 && m.mon_height > 0 => (m.mon_left, m.mon_top, m.mon_width, m.mon_height),
		_ => (0, 0, primary.0.max(1), primary.1.max(1)),
	};
	let px = left + ((x.clamp(0.0, 1.0) * w as f32) as i32).min(w - 1);
	let py = top + ((y.clamp(0.0, 1.0) * h as f32) as i32).min(h - 1);
	POINT { x: px, y: py }
}

/// The pointer-flag steps that take the pen from `prev` to `next` (each is one injection,
/// paired with the state whose tool / buttons it carries). Leaving range or switching tool
/// (pen ↔ eraser) lifts and leaves first; entering hovers once before a touch-down.
pub(crate) fn pen_steps(prev: &PenState, next: &PenState) -> Vec<(u32, PenState)> {
	let mut steps = Vec::new();
	let mut cur = *prev;
	let at = |s: &PenState| PenState { x: next.x, y: next.y, ..*s };
	if cur.in_range && (!next.in_range || cur.eraser != next.eraser) {
		if cur.contact {
			steps.push((POINTER_FLAG_UP | POINTER_FLAG_INRANGE, PenState { contact: false, pressure: 0.0, ..at(&cur) }));
		}
		steps.push((POINTER_FLAG_UPDATE, PenState { in_range: false, contact: false, pressure: 0.0, ..at(&cur) }));
		cur = PenState::default();
	}
	if !next.in_range {
		return steps;
	}
	if !cur.in_range && next.contact {
		steps.push((POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE, PenState { contact: false, pressure: 0.0, ..*next }));
		cur = PenState { contact: false, ..*next };
	}
	let buttons = |s: &PenState| {
		let mut f = 0;
		if s.contact {
			f |= POINTER_FLAG_FIRSTBUTTON;
		}
		if s.barrel {
			f |= POINTER_FLAG_SECONDBUTTON;
		}
		f
	};
	let f = match (cur.contact, next.contact) {
		(false, true) => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
		(true, true) => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
		(true, false) => POINTER_FLAG_UP | POINTER_FLAG_INRANGE,
		(false, false) => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE,
	};
	steps.push((f | buttons(next), *next));
	steps
}

/// One contact of an injected pointer frame as plain data: what [`WinSink`] hands its
/// [`PointerBackend`] (and what the host forwards to the SYSTEM helper, which injects it on
/// the secure desktop). Coordinates are physical virtual-desktop pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SynthContact {
	/// Pen: always 0. Touch: the slot (0..[`MAX_TOUCH`]).
	pub id: u32,
	/// `POINTER_FLAG_*`.
	pub flags: u32,
	pub x: i32,
	pub y: i32,
	/// 0..=1024 (pen: 0 while hovering; touch: 512 when the client has no pressure).
	pub pressure: u32,
	/// Pen rotation, 0..360 degrees.
	pub rotation: u32,
	/// Pen tilt, -90..=90 degrees.
	pub tilt_x: i32,
	pub tilt_y: i32,
	/// `PEN_FLAG_*` (pen only).
	pub pen_flags: u32,
}

/// Where [`WinSink`] injects: this process's synthetic pointer devices ([`LocalDevices`]) or
/// the in-session SYSTEM helper (the desktop app's `pulsar-sysagent` path), which reaches
/// UAC prompts and the lock screen.
pub trait PointerBackend {
	/// Inject one frame on the pen device (`pen`, exactly one contact) or the touch device.
	/// `false` = the injection failed.
	fn inject(&mut self, pen: bool, contacts: &[SynthContact]) -> bool;
}

/// A [`SynthContact`] as the `POINTER_TYPE_INFO` that `InjectSyntheticPointerInput` takes.
pub fn pointer_info(pen: bool, frame: u32, c: &SynthContact) -> POINTER_TYPE_INFO {
	let at = POINT { x: c.x, y: c.y };
	if pen {
		let mut p = POINTER_PEN_INFO::default();
		p.pointerInfo.pointerType = PT_PEN;
		p.pointerInfo.pointerId = c.id;
		p.pointerInfo.frameId = frame;
		p.pointerInfo.pointerFlags = c.flags;
		p.pointerInfo.ptPixelLocation = at;
		p.penMask = PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
		p.penFlags = c.pen_flags;
		p.pressure = c.pressure;
		p.rotation = c.rotation;
		p.tiltX = c.tilt_x;
		p.tiltY = c.tilt_y;
		POINTER_TYPE_INFO { r#type: PT_PEN, Anonymous: POINTER_TYPE_INFO_0 { penInfo: p } }
	} else {
		let mut t = POINTER_TOUCH_INFO::default();
		t.pointerInfo.pointerType = PT_TOUCH;
		t.pointerInfo.pointerId = c.id;
		t.pointerInfo.frameId = frame;
		t.pointerInfo.pointerFlags = c.flags;
		t.pointerInfo.ptPixelLocation = at;
		t.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
		t.rcContact = RECT { left: at.x - 4, top: at.y - 4, right: at.x + 4, bottom: at.y + 4 };
		t.orientation = 90;
		t.pressure = c.pressure;
		POINTER_TYPE_INFO { r#type: PT_TOUCH, Anonymous: POINTER_TYPE_INFO_0 { touchInfo: t } }
	}
}

/// This process's synthetic `PT_PEN` / `PT_TOUCH` devices (created on first use).
pub struct LocalDevices {
	api: Api,
	pen_dev: Option<HSYNTHETICPOINTERDEVICE>,
	touch_dev: Option<HSYNTHETICPOINTERDEVICE>,
	/// Last frame id (monotonic, as real digitizers stamp it).
	frame: u32,
	logged: bool,
}

// SAFETY: the device handles are only used on the worker thread that created them; the
// sink is built there (`PenTouch::spawn`'s factory) and never leaves it.
unsafe impl Send for LocalDevices {}

impl LocalDevices {
	/// `None` when this Windows has no synthetic pointer API (before 10 1809).
	pub fn new() -> Option<Self> {
		Some(Self { api: Api::load()?, pen_dev: None, touch_dev: None, frame: 0, logged: false })
	}

	fn device(&mut self, pen: bool) -> Option<HSYNTHETICPOINTERDEVICE> {
		let slot = if pen { &mut self.pen_dev } else { &mut self.touch_dev };
		if slot.is_none() {
			let (ty, max) = if pen { (PT_PEN, 1) } else { (PT_TOUCH, MAX_TOUCH as u32) };
			// SAFETY: plain value arguments; a null return = failure.
			let h = unsafe { (self.api.create)(ty, max, POINTER_FEEDBACK_DEFAULT) };
			if h.is_null() {
				if !self.logged {
					self.logged = true;
					tracing::warn!(err = %std::io::Error::last_os_error(), pen, "CreateSyntheticPointerDevice failed — pen/touch not injected");
				}
				return None;
			}
			tracing::info!(pen, "synthetic pointer device created");
			*slot = Some(h);
		}
		*slot
	}
}

impl PointerBackend for LocalDevices {
	fn inject(&mut self, pen: bool, contacts: &[SynthContact]) -> bool {
		if contacts.is_empty() {
			return true;
		}
		let Some(dev) = self.device(pen) else { return false };
		self.frame = self.frame.wrapping_add(1);
		let infos: Vec<POINTER_TYPE_INFO> = contacts.iter().map(|c| pointer_info(pen, self.frame, c)).collect();
		// SAFETY: `infos` is a valid slice of initialized POINTER_TYPE_INFO for this device.
		unsafe { (self.api.inject)(dev, infos.as_ptr(), infos.len() as u32) != 0 }
	}
}

impl Drop for LocalDevices {
	fn drop(&mut self) {
		for h in [self.pen_dev.take(), self.touch_dev.take()].into_iter().flatten() {
			// SAFETY: a handle CreateSyntheticPointerDevice returned, destroyed once.
			unsafe { (self.api.destroy)(h) };
		}
	}
}

/// The Windows [`Sink`]: turns pen / touch edges into synthetic pointer frames for a
/// [`PointerBackend`].
pub struct WinSink<B: PointerBackend = LocalDevices> {
	backend: B,
	geom: Option<MonitorRect>,
	logged: bool,
}

impl WinSink<LocalDevices> {
	/// In-process injection; `None` when this Windows has no synthetic pointer API.
	pub fn new() -> Option<Self> {
		Some(Self::with_backend(LocalDevices::new()?))
	}
}

impl<B: PointerBackend> WinSink<B> {
	/// Any backend. Call on the worker thread: it makes that thread per-monitor-v2 DPI aware
	/// (pixel coordinates are physical).
	pub fn with_backend(backend: B) -> Self {
		per_monitor_dpi();
		Self { backend, geom: None, logged: false }
	}

	fn primary() -> (i32, i32) {
		// SAFETY: no arguments besides the metric index.
		unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
	}

	fn inject(&mut self, pen: bool, contacts: &[SynthContact]) {
		if contacts.is_empty() {
			return;
		}
		if !self.backend.inject(pen, contacts) && !self.logged {
			self.logged = true;
			tracing::warn!(err = %std::io::Error::last_os_error(), pen, "pen/touch pointer injection failed");
		}
	}

	fn pen_contact(&self, flags: u32, s: &PenState) -> SynthContact {
		let at = to_px(self.geom, Self::primary(), s.x, s.y);
		pen_contact(flags, s, at)
	}

	fn touch_contact(&self, slot: usize, flags: u32, pt: &TouchPoint) -> SynthContact {
		let at = to_px(self.geom, Self::primary(), pt.x, pt.y);
		SynthContact {
			id: slot as u32,
			flags,
			x: at.x,
			y: at.y,
			pressure: if pt.pressure > 0.0 { (pt.pressure * 1024.0).round().clamp(1.0, 1024.0) as u32 } else { 512 },
			..Default::default()
		}
	}
}

/// The pen contact for one injection step at pixel `at`.
pub(crate) fn pen_contact(flags: u32, s: &PenState, at: POINT) -> SynthContact {
	let mut pen_flags = 0;
	if s.eraser {
		pen_flags |= PEN_FLAG_INVERTED;
		if s.contact {
			pen_flags |= PEN_FLAG_ERASER;
		}
	}
	if s.barrel {
		pen_flags |= PEN_FLAG_BARREL;
	}
	SynthContact {
		id: 0,
		flags,
		x: at.x,
		y: at.y,
		pressure: if s.contact { (s.pressure * 1024.0).round().clamp(1.0, 1024.0) as u32 } else { 0 },
		rotation: (s.twist.max(0.0) as u32) % 360,
		tilt_x: (s.tilt_x.round() as i32).clamp(-90, 90),
		tilt_y: (s.tilt_y.round() as i32).clamp(-90, 90),
		pen_flags,
	}
}

const TOUCH_DOWN: u32 = POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
const TOUCH_MOVE: u32 = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;

impl<B: PointerBackend> Sink for WinSink<B> {
	type Geom = Option<MonitorRect>;
	fn set_geom(&mut self, g: Self::Geom) {
		self.geom = g;
	}
	fn pen(&mut self, prev: &PenState, next: &PenState) {
		for (flags, s) in pen_steps(prev, next) {
			let c = self.pen_contact(flags, &s);
			self.inject(true, &[c]);
		}
	}
	fn touch(&mut self, _now: Instant, _frame: &TouchFrame, changes: &[SlotChange]) {
		let contacts: Vec<SynthContact> = changes
			.iter()
			.map(|c| {
				let f = match c.phase {
					Contact::Down => TOUCH_DOWN,
					Contact::Move => TOUCH_MOVE,
					Contact::Up => POINTER_FLAG_UP,
				};
				self.touch_contact(c.slot, f, &c.pt)
			})
			.collect();
		self.inject(false, &contacts);
	}
	fn refresh(&mut self, pen: &PenState, touch: &[(usize, TouchPoint)]) {
		if pen.in_range {
			let mut f = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE;
			if pen.contact {
				f |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
			}
			if pen.barrel {
				f |= POINTER_FLAG_SECONDBUTTON;
			}
			let c = self.pen_contact(f, pen);
			self.inject(true, &[c]);
		}
		if !touch.is_empty() {
			let contacts: Vec<_> = touch.iter().map(|(slot, pt)| self.touch_contact(*slot, TOUCH_MOVE, pt)).collect();
			self.inject(false, &contacts);
		}
	}
	fn refresh_every(&self) -> Option<Duration> {
		Some(REFRESH)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pen_hover_down_drag_up_leave() {
		let out = PenState::default();
		let hover = PenState { x: 0.5, y: 0.5, in_range: true, ..Default::default() };
		let tip = PenState { contact: true, pressure: 0.5, ..hover };
		// Straight from out of range to the tip: hover once, then down.
		let s = pen_steps(&out, &tip);
		assert_eq!(s.len(), 2);
		assert_eq!(s[0].0, POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE);
		assert!(!s[0].1.contact);
		assert_eq!(s[1].0, TOUCH_DOWN | POINTER_FLAG_FIRSTBUTTON);
		assert_eq!(pen_steps(&tip, &tip)[0].0, TOUCH_MOVE | POINTER_FLAG_FIRSTBUTTON);
		assert_eq!(pen_steps(&tip, &hover), vec![(POINTER_FLAG_UP | POINTER_FLAG_INRANGE, hover)]);
		// Leaving while touching: up, then the out-of-range update (no INRANGE).
		let s = pen_steps(&tip, &out);
		assert_eq!(s.iter().map(|s| s.0).collect::<Vec<_>>(), vec![POINTER_FLAG_UP | POINTER_FLAG_INRANGE, POINTER_FLAG_UPDATE]);
		assert!(pen_steps(&out, &out).is_empty());
	}

	#[test]
	fn switching_to_the_eraser_leaves_and_re_enters() {
		let pen = PenState { x: 0.2, y: 0.2, in_range: true, ..Default::default() };
		let eraser = PenState { eraser: true, ..pen };
		let s = pen_steps(&pen, &eraser);
		assert_eq!(s.iter().map(|s| s.0).collect::<Vec<_>>(), vec![POINTER_FLAG_UPDATE, POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE]);
		assert!(!s[0].1.eraser && s[1].1.eraser);
	}

	#[test]
	fn pen_contact_carries_tool_buttons_and_pressure() {
		let s = PenState { x: 0.1, y: 0.1, in_range: true, contact: true, pressure: 0.5, eraser: true, barrel: true, twist: 370.0, tilt_x: 120.0, ..Default::default() };
		let c = pen_contact(TOUCH_MOVE, &s, POINT { x: 3, y: 4 });
		assert_eq!((c.x, c.y, c.pressure, c.rotation, c.tilt_x), (3, 4, 512, 10, 90));
		assert_eq!(c.pen_flags, PEN_FLAG_INVERTED | PEN_FLAG_ERASER | PEN_FLAG_BARREL);
		let hover = pen_contact(POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE, &PenState { contact: false, ..s }, POINT { x: 0, y: 0 });
		assert_eq!((hover.pressure, hover.pen_flags), (0, PEN_FLAG_INVERTED | PEN_FLAG_BARREL));
	}

	#[test]
	fn coordinates_land_on_the_streamed_monitor() {
		let m = MonitorRect {
			mon_left: -1920,
			mon_top: 0,
			mon_width: 1920,
			mon_height: 1080,
			virt_left: -1920,
			virt_top: 0,
			virt_width: 3840,
			virt_height: 1080,
		};
		let p = to_px(Some(m), (2560, 1440), 0.5, 1.0);
		assert_eq!((p.x, p.y), (-960, 1079));
		let p = to_px(None, (2560, 1440), 1.0, 0.0);
		assert_eq!((p.x, p.y), (2559, 0));
	}
}
