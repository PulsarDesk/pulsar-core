//! macOS **per-window** mouse + keyboard injection — the macOS side of same-host
//! co-op. Mirrors the Windows `window_win::WindowInput` surface so the host's
//! `on_input` closure needs no per-OS branching.
//!
//! ## How it works
//!
//! The target is a **CGWindowID** (what ScreenCaptureKit / `CGWindowList` name a
//! window by). `CGWindowListCopyWindowInfo` maps it to its owning process (PID) and
//! its on-screen bounds (global points). Events are then built with CoreGraphics
//! and posted with **`CGEventPostToPid`**, which delivers them into that one
//! process's event queue — the system cursor and the front app are untouched, so
//! two co-op panes can each drive their own app. Mouse events also carry the window
//! number in `kCGMouseEventWindowUnderMousePointer[ThatCanHandleThisEvent]`, which is
//! what AppKit uses to route a mouse event to a window that is not under the real
//! cursor. When the target IS the front window, mouse events go through the HID tap
//! at the window's global position instead (identical to real input; also reaches
//! games); the host sends a front window's keyboard through the global injector.
//!
//! ## HONEST CEILING
//!
//! * Needs the same **Accessibility** grant as the global injector (else the OS
//!   silently drops every posted event).
//! * `CGEventPostToPid` reaches apps that read the **AppKit/NSEvent** queue (Cocoa and
//!   most cross-platform toolkits, SDL keyboard, many windowed games). It does NOT
//!   reach apps that read **IOHID / GameController** directly or use a CGEventTap of
//!   their own at the HID level, and background mouse delivery depends on the app
//!   accepting events for a non-key window (most Cocoa views do for clicks/scroll;
//!   some games only honour the key window).
//! * Several windows of ONE process share that process's queue: events reach the
//!   right window through the window-number fields above, but keyboard input goes to
//!   the process's key window.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::event::{
	CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
	ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
	copy_window_info, kCGWindowBounds, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
	kCGWindowListOptionIncludingWindow, kCGWindowListOptionOnScreenOnly, kCGWindowNumber,
	kCGWindowOwnerPID, CGWindowID,
};
use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::macos::{evdev_modifier_flag, evdev_to_vk};
use super::window_common::{clamp_to_area, norm_to_area, ScrollAcc};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
	fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

/// Window bounds / front state are re-read at most this often (a CGWindowList query
/// per mouse-move event would cost far more than the event itself).
const CACHE_TTL: Duration = Duration::from_millis(200);

/// One window's entry from `CGWindowListCopyWindowInfo`.
struct WinInfo {
	pid: i32,
	bounds: CGRect,
}

fn dict_i64(d: &CFDictionary<CFString, CFType>, key: core_foundation::string::CFStringRef) -> Option<i64> {
	let k = unsafe { CFString::wrap_under_get_rule(key) };
	d.find(&k)?.downcast::<CFNumber>()?.to_i64()
}

fn dicts(arr: &CFArray) -> Vec<CFDictionary<CFString, CFType>> {
	(0..arr.len())
		.filter_map(|i| arr.get(i))
		.map(|p| unsafe { CFDictionary::wrap_under_get_rule(*p as CFDictionaryRef) })
		.collect()
}

fn window_info(id: CGWindowID) -> Option<WinInfo> {
	let arr = copy_window_info(kCGWindowListOptionIncludingWindow, id)?;
	let d = dicts(&arr).into_iter().find(|d| dict_i64(d, unsafe { kCGWindowNumber }) == Some(id as i64))?;
	let pid = dict_i64(&d, unsafe { kCGWindowOwnerPID })? as i32;
	let key = unsafe { CFString::wrap_under_get_rule(kCGWindowBounds) };
	let b = d.find(&key)?;
	let mut bounds = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
	let ok = unsafe { CGRectMakeWithDictionaryRepresentation(b.as_CFTypeRef() as CFDictionaryRef, &mut bounds) };
	ok.then_some(WinInfo { pid, bounds })
}

/// The pid of the frontmost (active) application, from `NSWorkspace`.
fn frontmost_pid() -> Option<i32> {
	use objc2_app_kit::NSWorkspace;
	let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
	let pid = app.processIdentifier();
	(pid > 0).then_some(pid)
}

/// The top normal (layer 0) on-screen window of process `pid` — CGWindowList returns
/// on-screen windows front to back, so the first match is that app's front window.
fn top_window_of(pid: i32) -> Option<i64> {
	let arr = copy_window_info(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, 0)?;
	dicts(&arr)
		.into_iter()
		.find(|d| {
			dict_i64(d, unsafe { kCGWindowLayer }) == Some(0)
				&& dict_i64(d, unsafe { kCGWindowOwnerPID }) == Some(pid as i64)
		})
		.and_then(|d| dict_i64(&d, unsafe { kCGWindowNumber }))
}

/// Is window `id` (owned by `pid`) the key window of the active app? The active app
/// must be `pid`, and `id` must be that app's top window. The first on-screen layer-0
/// window alone is not enough: it can be a non-activating window of another app.
fn is_front(id: CGWindowID, pid: i32) -> bool {
	frontmost_pid() == Some(pid) && top_window_of(pid) == Some(id as i64)
}

/// Backing scale factor (pixels per point) of the display under `pt`; 1.0 if unknown.
/// Client relative-mouse deltas are in host PIXELS (what the client sees in the stream),
/// window coordinates here are POINTS, so a Retina host divides by 2.
fn backing_scale_at(pt: CGPoint) -> f64 {
	let id = CGDisplay::displays_with_point(pt, 1)
		.ok()
		.and_then(|(ids, n)| (n > 0).then(|| ids.first().copied()).flatten())
		.unwrap_or_else(|| CGDisplay::main().id);
	CGDisplay::new(id)
		.display_mode()
		.map(|m| (m.width(), m.pixel_width()))
		.filter(|&(w, px)| w > 0 && px > 0)
		.map_or(1.0, |(w, px)| px as f64 / w as f64)
}

/// One `CACHE_TTL`-cached boolean query.
fn cached(slot: &Cell<Option<(Instant, bool)>>, query: impl FnOnce() -> bool) -> bool {
	if let Some((t, v)) = slot.get() {
		if t.elapsed() < CACHE_TTL {
			return v;
		}
	}
	let v = query();
	slot.set(Some((Instant::now(), v)));
	v
}

fn button_events(button: u8, down: bool) -> (CGMouseButton, CGEventType) {
	match (button, down) {
		(1, true) => (CGMouseButton::Right, CGEventType::RightMouseDown),
		(1, false) => (CGMouseButton::Right, CGEventType::RightMouseUp),
		(2, true) => (CGMouseButton::Center, CGEventType::OtherMouseDown),
		(2, false) => (CGMouseButton::Center, CGEventType::OtherMouseUp),
		(_, true) => (CGMouseButton::Left, CGEventType::LeftMouseDown),
		(_, false) => (CGMouseButton::Left, CGEventType::LeftMouseUp),
	}
}

pub struct WindowInput {
	id: CGWindowID,
	pid: i32,
	bounds: CGRect,
	bounds_at: Instant,
	/// Pixels per point of the window's display (refreshed with `bounds`).
	scale: f64,
	/// Cached `in_front` / `is_alive` answers and when they were read (see `CACHE_TTL`):
	/// the host asks both on every input event, and each is a window-server query.
	front: Cell<Option<(Instant, bool)>>,
	alive: Cell<Option<(Instant, bool)>>,
	/// Last pointer position in window-local points.
	last_x: f64,
	last_y: f64,
	held_buttons: HashSet<u8>,
	held_keys: HashSet<u16>,
	flags: CGEventFlags,
	caps_lock: bool,
	scroll: ScrollAcc,
}

impl WindowInput {
	/// Target CGWindowID `id`. `None` if no such window exists (the caller keeps the
	/// global injector).
	pub fn new(id: i64) -> Option<Self> {
		let id = CGWindowID::try_from(id).ok().filter(|&w| w != 0)?;
		let info = window_info(id)?;
		CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
		Some(Self {
			id,
			pid: info.pid,
			scale: backing_scale_at(info.bounds.origin),
			bounds: info.bounds,
			bounds_at: Instant::now(),
			front: Cell::new(None),
			alive: Cell::new(None),
			last_x: 0.0,
			last_y: 0.0,
			held_buttons: HashSet::new(),
			held_keys: HashSet::new(),
			flags: CGEventFlags::empty(),
			caps_lock: false,
			scroll: ScrollAcc::default(),
		})
	}

	fn source() -> Option<CGEventSource> {
		CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
	}

	/// Is the target window still alive? (Cached for `CACHE_TTL`.)
	pub fn is_alive(&self) -> bool {
		cached(&self.alive, || window_info(self.id).is_some())
	}

	/// Is the target the key window of the active app? (Cached for `CACHE_TTL`.)
	pub fn in_front(&self) -> bool {
		cached(&self.front, || is_front(self.id, self.pid))
	}

	fn refresh_bounds(&mut self) {
		if self.bounds_at.elapsed() > CACHE_TTL {
			if let Some(i) = window_info(self.id) {
				self.bounds = i.bounds;
				self.pid = i.pid;
				self.scale = backing_scale_at(i.bounds.origin);
			}
			self.bounds_at = Instant::now();
		}
	}

	/// The cached window-local point in global display points.
	fn global(&self) -> CGPoint {
		CGPoint::new(self.bounds.origin.x + self.last_x, self.bounds.origin.y + self.last_y)
	}

	/// Deliver one event: HID tap when the window is in front, else its process.
	fn post(&self, ev: &CGEvent, mouse: bool) {
		if mouse {
			let id = self.id as i64;
			ev.set_integer_value_field(EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER, id);
			ev.set_integer_value_field(
				EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER_THAT_CAN_HANDLE_THIS_EVENT,
				id,
			);
			if self.in_front() {
				ev.post(CGEventTapLocation::HID);
				return;
			}
		}
		ev.post_to_pid(self.pid);
	}

	fn move_type(&self) -> CGEventType {
		if self.held_buttons.contains(&1) {
			CGEventType::RightMouseDragged
		} else if self.held_buttons.contains(&2) {
			CGEventType::OtherMouseDragged
		} else if self.held_buttons.contains(&0) {
			CGEventType::LeftMouseDragged
		} else {
			CGEventType::MouseMoved
		}
	}

	fn post_move(&mut self) {
		let btn = if self.held_buttons.contains(&1) {
			CGMouseButton::Right
		} else if self.held_buttons.contains(&2) {
			CGMouseButton::Center
		} else {
			CGMouseButton::Left
		};
		let ty = self.move_type();
		let pt = self.global();
		if let Some(ev) = Self::source().and_then(|s| CGEvent::new_mouse_event(s, ty, pt, btn).ok()) {
			ev.set_flags(self.flags);
			self.post(&ev, true);
		}
	}

	/// Move to a normalized (0..1) position on the target window.
	pub fn pointer(&mut self, x: f64, y: f64) {
		self.refresh_bounds();
		(self.last_x, self.last_y) = norm_to_area(x, y, self.bounds.size.width, self.bounds.size.height);
		self.post_move();
	}

	/// Advance the cached pointer by a raw pixel delta (scaled to points).
	pub fn pointer_relative(&mut self, dx: f64, dy: f64) {
		self.refresh_bounds();
		let k = 1.0 / self.scale.max(1.0);
		(self.last_x, self.last_y) = clamp_to_area(
			self.last_x + dx * k,
			self.last_y + dy * k,
			self.bounds.size.width,
			self.bounds.size.height,
		);
		self.post_move();
	}

	/// Press/release a mouse button (0=left, 1=right, 2=middle) at the cached point.
	pub fn button(&mut self, button: u8, down: bool) {
		if down {
			self.held_buttons.insert(button);
		} else {
			self.held_buttons.remove(&button);
		}
		self.refresh_bounds();
		let (b, ty) = button_events(button, down);
		let pt = self.global();
		if let Some(ev) = Self::source().and_then(|s| CGEvent::new_mouse_event(s, ty, pt, b).ok()) {
			ev.set_flags(self.flags);
			self.post(&ev, true);
		}
	}

	/// Scroll by a browser-wheel delta (100 px per line), at the cached point.
	pub fn scroll(&mut self, dx: f64, dy: f64) {
		let (v, h) = self.scroll.push(dx, dy);
		if v == 0 && h == 0 {
			return;
		}
		let pt = self.global();
		if let Some(ev) = Self::source()
			.and_then(|s| CGEvent::new_scroll_event(s, ScrollEventUnit::LINE, 2, -v, -h, 0).ok())
		{
			ev.set_location(pt);
			self.post(&ev, true);
		}
	}

	/// Press/release a key by evdev keycode, posted to the window's process.
	pub fn key(&mut self, code: u32, down: bool) {
		if code == 58 && down && !self.held_keys.contains(&0x39) {
			self.caps_lock = !self.caps_lock;
			self.flags.set(CGEventFlags::CGEventFlagAlphaShift, self.caps_lock);
		}
		if let Some(flag) = evdev_modifier_flag(code) {
			self.flags.set(flag, down);
		}
		let Some(vk) = evdev_to_vk(code) else {
			return;
		};
		if down {
			self.held_keys.insert(vk);
		} else {
			self.held_keys.remove(&vk);
		}
		if let Some(ev) = Self::source().and_then(|s| CGEvent::new_keyboard_event(s, vk, down).ok()) {
			ev.set_flags(self.flags);
			self.post(&ev, false);
		}
	}

	/// Type a resolved Unicode character (layout-independent), posted to the process.
	pub fn type_char(&mut self, c: char) {
		let mut buf = [0u16; 2];
		let units = c.encode_utf16(&mut buf);
		for down in [true, false] {
			if let Some(ev) = Self::source().and_then(|s| CGEvent::new_keyboard_event(s, 0, down).ok()) {
				ev.set_string_from_utf16_unchecked(units);
				self.post(&ev, false);
			}
		}
	}

	/// Release every held key + button on the target window. Idempotent.
	pub fn flush_held(&mut self) {
		for b in self.held_buttons.clone() {
			self.button(b, false);
		}
		self.flags = CGEventFlags::empty();
		self.caps_lock = false;
		for vk in std::mem::take(&mut self.held_keys) {
			if let Some(ev) = Self::source().and_then(|s| CGEvent::new_keyboard_event(s, vk, false).ok()) {
				self.post(&ev, false);
			}
		}
	}
}

impl Drop for WindowInput {
	fn drop(&mut self) {
		if self.is_alive() {
			self.flush_held();
		}
	}
}

// Held in the host's `on_input` closure state (a tokio task) → must be Send. All
// fields are plain data (no CGEventSource is stored, see macos.rs).
const _: fn() = || {
	fn assert_send<T: Send>() {}
	assert_send::<WindowInput>();
};
