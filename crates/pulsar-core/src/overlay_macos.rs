//! macOS: turn an app-owned `NSWindow` into a system-wide overlay — the desktop host's
//! remote-cursor "ghost" (a client's labelled pointer, desktop `remote_cursor.rs`).
//!
//! A Tauri `always_on_top` window sits at the floating level and only on the Space it was
//! opened on: it vanishes when the host user switches Spaces or runs an app full screen, and
//! the menu bar / Dock cover it. [`make_overlay_window`] moves it to the status-window level
//! and lets it join every Space, full-screen ones included, without taking part in window
//! cycling (Cmd-`) or Mission Control.
//!
//! Untested on macOS hardware.

use objc2::msg_send;
use objc2::runtime::AnyObject;

/// `NSWindowCollectionBehavior` bits (AppKit, stable since 10.5–10.7).
const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
const STATIONARY: usize = 1 << 4;
const IGNORES_CYCLE: usize = 1 << 6;
const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
/// `NSStatusWindowLevel` (= `kCGStatusWindowLevel`): above the menu bar (24) and the Dock (20).
const STATUS_WINDOW_LEVEL: isize = 25;

/// The collection behaviour an overlay gets, on top of what the window already had.
fn overlay_behavior(current: usize) -> usize {
	current | CAN_JOIN_ALL_SPACES | STATIONARY | IGNORES_CYCLE | FULL_SCREEN_AUXILIARY
}

/// Make `ns_window` (an `NSWindow*`, e.g. Tauri's `WebviewWindow::ns_window()`) a system-wide,
/// click-through overlay.
///
/// # Safety
/// `ns_window` must be a live `NSWindow`, and this must run on the main thread (AppKit).
pub unsafe fn make_overlay_window(ns_window: *mut std::ffi::c_void) {
	if ns_window.is_null() {
		return;
	}
	// SAFETY: the caller guarantees a live NSWindow on the main thread; every selector below
	// is plain NSWindow API with the AppKit argument types (NSUInteger / NSInteger / BOOL).
	let w = unsafe { &*(ns_window as *const AnyObject) };
	unsafe {
		let cur: usize = msg_send![w, collectionBehavior];
		let _: () = msg_send![w, setCollectionBehavior: overlay_behavior(cur)];
		let _: () = msg_send![w, setLevel: STATUS_WINDOW_LEVEL];
		let _: () = msg_send![w, setHidesOnDeactivate: false];
		let _: () = msg_send![w, setIgnoresMouseEvents: true];
		let _: () = msg_send![w, setHasShadow: false];
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn overlay_joins_all_spaces_and_keeps_existing_bits() {
		let b = overlay_behavior(1 << 2);
		assert_eq!(b & (1 << 2), 1 << 2);
		assert_eq!(b & CAN_JOIN_ALL_SPACES, CAN_JOIN_ALL_SPACES);
		assert_eq!(b & FULL_SCREEN_AUXILIARY, FULL_SCREEN_AUXILIARY);
		assert_eq!(b & IGNORES_CYCLE, IGNORES_CYCLE);
	}
}
