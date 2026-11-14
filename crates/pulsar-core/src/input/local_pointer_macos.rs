//! macOS host: notice the person AT the host using their own mouse (the "host takes the mouse
//! back" hold + the client's "Host" pointer marker, desktop `host::takeover`). The Windows
//! analogue is a `WH_MOUSE_LL` hook that skips `LLMHF_INJECTED` events.
//!
//! A **listen-only** `CGEventTap` at the session tap sees every mouse move / click / scroll
//! after the HID layer. Pulsar's own injected events (`input::macos`) carry
//! [`super::macos::INJECTED_TAG`] in `kCGEventSourceUserData`, so they are skipped; as a second
//! guard, events whose source process is this process are skipped too. Listen-only mouse taps
//! need no extra TCC grant beyond the Accessibility permission injection already requires (on
//! some macOS versions "Input Monitoring" is asked for; without it `CGEventTapCreate` returns
//! NULL and [`watch_local_pointer`] reports an error — nothing is detected, remote input always
//! applies, as before).
//!
//! Untested on macOS hardware.

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
	CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
	CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::sync::atomic::{AtomicPtr, Ordering};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
	fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

/// The live tap's mach port, so the callback can re-enable it after the system disabled it
/// (`kCGEventTapDisabledByTimeout` / `…ByUserInput`).
static TAP: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Is an event with this `kCGEventSourceUserData` / source pid the local user's (not ours)?
fn is_local(user_data: i64, source_pid: i64, own_pid: i64) -> bool {
	user_data != super::macos::INJECTED_TAG && source_pid != own_pid
}

/// Where `(x, y)` (global display points) falls in the rect `(ox, oy, w, h)`, as 0..1;
/// `None` when outside it or the rect is empty.
fn fraction_in(x: f64, y: f64, ox: f64, oy: f64, w: f64, h: f64) -> Option<(f32, f32)> {
	if w <= 0.0 || h <= 0.0 {
		return None;
	}
	let (nx, ny) = ((x - ox) / w, (y - oy) / h);
	((0.0..=1.0).contains(&nx) && (0.0..=1.0).contains(&ny)).then_some((nx as f32, ny as f32))
}

/// The host pointer within the main display (the one `DesktopInput::pointer` maps the remote
/// pointer onto and the capture streams by default), 0..1; `None` = elsewhere / unreadable.
pub fn pointer_on_main_display() -> Option<(f32, f32)> {
	let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
	let p = CGEvent::new(src).ok()?.location();
	let b = CGDisplay::main().bounds();
	fraction_in(p.x, p.y, b.origin.x, b.origin.y, b.size.width, b.size.height)
}

/// Watch the local mouse; `on_local` runs for each move / click / scroll that did not come
/// from Pulsar. Blocks the calling thread (it runs its CFRunLoop) for the process lifetime;
/// `Err` = the tap could not be created (missing permission / no window-server session).
pub fn watch_local_pointer(on_local: impl Fn() + Send + 'static) -> Result<(), String> {
	let own_pid = std::process::id() as i64;
	let events = vec![
		CGEventType::MouseMoved,
		CGEventType::LeftMouseDown,
		CGEventType::LeftMouseUp,
		CGEventType::LeftMouseDragged,
		CGEventType::RightMouseDown,
		CGEventType::RightMouseUp,
		CGEventType::RightMouseDragged,
		CGEventType::OtherMouseDown,
		CGEventType::OtherMouseUp,
		CGEventType::OtherMouseDragged,
		CGEventType::ScrollWheel,
	];
	let tap = CGEventTap::new(
		CGEventTapLocation::Session,
		CGEventTapPlacement::TailAppendEventTap,
		CGEventTapOptions::ListenOnly,
		events,
		move |_proxy, ty, ev| {
			match ty {
				CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
					let port = TAP.load(Ordering::Acquire);
					if !port.is_null() {
						// SAFETY: the port belongs to the tap, alive while this callback runs.
						unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
					}
				}
				_ => {
					let user = ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);
					let pid = ev.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID);
					if is_local(user, pid, own_pid) {
						on_local();
					}
				}
			}
			CallbackResult::Keep
		},
	)
	.map_err(|_| "CGEventTapCreate failed (permission / no window server)".to_string())?;
	TAP.store(tap.mach_port().as_concrete_TypeRef() as *mut _, Ordering::Release);
	let source = tap
		.mach_port()
		.create_runloop_source(0)
		.map_err(|_| "event tap run-loop source failed".to_string())?;
	// SAFETY: kCFRunLoopCommonModes is a static CF string constant.
	CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
	tap.enable();
	CFRunLoop::run_current();
	TAP.store(std::ptr::null_mut(), Ordering::Release);
	drop(tap);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn injected_and_own_events_are_not_local() {
		let tag = super::super::macos::INJECTED_TAG;
		assert!(is_local(0, 0, 42));
		assert!(is_local(0, 99, 42)); // another process (not Pulsar)
		assert!(!is_local(tag, 0, 42));
		assert!(!is_local(0, 42, 42));
	}

	#[test]
	fn fraction_maps_inside_only() {
		assert_eq!(fraction_in(960.0, 540.0, 0.0, 0.0, 1920.0, 1080.0), Some((0.5, 0.5)));
		assert_eq!(fraction_in(-1.0, 10.0, 0.0, 0.0, 1920.0, 1080.0), None);
		assert_eq!(fraction_in(2000.0, 10.0, 1920.0, 0.0, 1280.0, 800.0), Some((0.0625, 0.0125)));
		assert_eq!(fraction_in(1.0, 1.0, 0.0, 0.0, 0.0, 0.0), None);
	}
}
