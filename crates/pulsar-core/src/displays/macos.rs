//! macOS monitors: `CGGetActiveDisplayList` (+ the current/available `CGDisplayMode`s) for
//! geometry, `NSScreen.localizedName` for names, `CGDisplayRegisterReconfigurationCallback`
//! for change events. Untested on macOS hardware.
//!
//! AppKit's `NSScreen` is main-thread-only and [`list`] runs on worker threads, so names
//! come from a cache that is (re)filled ON the main thread: asynchronously via the main
//! dispatch queue when a display id has no cached name yet, and synchronously inside the
//! reconfiguration callback (which macOS delivers on the main thread). Never a synchronous
//! hop to the main queue — a host whose main thread waits on this one would deadlock. Until
//! the cache fills the name is empty and the app labels the monitor in the UI language
//! (`Monitor::builtin` tells a built-in panel apart).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use core_graphics::display::{CGDirectDisplayID, CGDisplay, CGDisplayMode};

use super::{normalize_modes, Monitor};

fn names() -> &'static Mutex<HashMap<u32, String>> {
	static N: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
	N.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read every `NSScreen`'s display id + localized name into the cache. Main thread only
/// (the marker proves it).
fn refresh_names(mtm: objc2::MainThreadMarker) {
	use objc2_app_kit::NSScreen;
	use objc2_foundation::{NSNumber, NSString};
	let key = NSString::from_str("NSScreenNumber");
	let screens = NSScreen::screens(mtm);
	let mut fresh = HashMap::new();
	for i in 0..screens.count() {
		let screen = screens.objectAtIndex(i);
		let desc = screen.deviceDescription();
		let Some(num) = desc.objectForKey(&key) else { continue };
		let Ok(num) = num.downcast::<NSNumber>() else { continue };
		let id = num.unsignedIntValue();
		let name = screen.localizedName().to_string();
		if !name.trim().is_empty() {
			fresh.insert(id, name);
		}
	}
	*names().lock().unwrap() = fresh;
}

/// Ask the main thread to refresh the name cache (at most one request in flight).
fn request_names() {
	static PENDING: AtomicBool = AtomicBool::new(false);
	if let Some(mtm) = objc2::MainThreadMarker::new() {
		refresh_names(mtm);
		return;
	}
	if PENDING.swap(true, Ordering::SeqCst) {
		return;
	}
	dispatch2::DispatchQueue::main().exec_async(|| {
		if let Some(mtm) = objc2::MainThreadMarker::new() {
			refresh_names(mtm);
		}
		PENDING.store(false, Ordering::SeqCst);
	});
}

pub(super) fn list() -> Vec<Monitor> {
	let Ok(ids) = CGDisplay::active_displays() else {
		return Vec::new();
	};
	let cached = names().lock().unwrap().clone();
	if ids.iter().any(|id| !cached.contains_key(id)) {
		request_names();
	}
	// CGGetActiveDisplayList puts the main display first; trust is_main() when one says so.
	let any_main = ids.iter().any(|&id| CGDisplay::new(id).is_main());
	ids.iter()
		.enumerate()
		.map(|(i, &id)| monitor(i, id, cached.get(&id).cloned(), any_main))
		.collect()
}

fn monitor(i: usize, id: CGDirectDisplayID, name: Option<String>, any_main: bool) -> Monitor {
	let d = CGDisplay::new(id);
	let b = d.bounds();
	let mode = d.display_mode();
	// The mode's PIXEL size (Retina backing), which is what avfoundation captures; the
	// bounds are in points.
	let (width, height) = match &mode {
		Some(m) if m.pixel_width() > 0 && m.pixel_height() > 0 => (m.pixel_width() as u32, m.pixel_height() as u32),
		_ => (d.pixels_wide() as u32, d.pixels_high() as u32),
	};
	// Built-in panels report 0 Hz (variable / ProMotion): unknown.
	let refresh_hz = mode.as_ref().map(|m| m.refresh_rate().round() as u32).unwrap_or(0);
	let modes = CGDisplayMode::all_display_modes(id, std::ptr::null())
		.map(|all| {
			normalize_modes(all.iter().map(|m| (m.pixel_width() as u32, m.pixel_height() as u32)))
		})
		.unwrap_or_default();
	Monitor {
		name: name.unwrap_or_default(),
		builtin: d.is_builtin(),
		x: b.origin.x.round() as i32,
		y: b.origin.y.round() as i32,
		logical_w: b.size.width.round().max(0.0) as u32,
		logical_h: b.size.height.round().max(0.0) as u32,
		width,
		height,
		primary: if any_main { d.is_main() } else { i == 0 },
		refresh_hz,
		modes,
		id,
	}
}

pub(super) fn pointer_in(idx: usize) -> Option<(f32, f32)> {
	use core_graphics::event::CGEvent;
	use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
	let id = *CGDisplay::active_displays().ok()?.get(idx)?;
	// An event created without a type carries the current pointer location (global points,
	// top-left origin — the same space as CGDisplayBounds).
	let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
	let p = CGEvent::new(src).ok()?.location();
	let b = CGDisplay::new(id).bounds();
	super::normalize_in(p.x, p.y, b.origin.x, b.origin.y, b.size.width, b.size.height)
}

fn on_change_slot() -> &'static Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>> {
	static S: OnceLock<Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>> = OnceLock::new();
	S.get_or_init(|| Mutex::new(None))
}

unsafe extern "C" fn reconfigured(_display: CGDirectDisplayID, flags: u32, _user: *const c_void) {
	// kCGDisplayBeginConfigurationFlag: the change is about to happen — wait for the
	// matching "done" callback, which carries the new state.
	if flags & 1 != 0 {
		return;
	}
	if let Some(mtm) = objc2::MainThreadMarker::new() {
		refresh_names(mtm);
	}
	if let Some(cb) = on_change_slot().lock().unwrap().as_ref() {
		cb();
	}
}

pub(super) fn watch(on_change: Box<dyn Fn() + Send + Sync + 'static>) -> bool {
	static REGISTERED: AtomicBool = AtomicBool::new(false);
	*on_change_slot().lock().unwrap() = Some(on_change);
	if REGISTERED.swap(true, Ordering::SeqCst) {
		return super::watching();
	}
	// Register on the main thread: the callbacks are delivered through the window-server
	// connection's main run loop. Async — never block a worker on the main queue.
	let register = || {
		let err = unsafe {
			core_graphics::display::CGDisplayRegisterReconfigurationCallback(reconfigured, std::ptr::null())
		};
		if err == 0 {
			super::set_watching(true);
			tracing::info!("display watch: CoreGraphics reconfiguration callback registered");
		} else {
			tracing::warn!(err, "display watch: CGDisplayRegisterReconfigurationCallback failed");
		}
	};
	if objc2::MainThreadMarker::new().is_some() {
		register();
	} else {
		dispatch2::DispatchQueue::main().exec_async(register);
	}
	super::watching()
}
