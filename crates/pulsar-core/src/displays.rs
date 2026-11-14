//! The host's monitors — enumeration and change notification — for the platforms whose
//! code the app crate can't check itself or that need protocol access the app crate doesn't
//! carry: **macOS** (CoreGraphics + AppKit names) and **Linux** (Wayland outputs; change
//! events from Wayland `wl_output` or X11 RandR). Windows enumerates through its own DXGI
//! path in the app (`pulsar_capture::list_displays`), so [`list`] is empty there.
//!
//! The index of a monitor in [`list`] is what travels in `StreamReq::display_idx`:
//! * macOS: the `CGGetActiveDisplayList` order — main display first (Apple documents the
//!   first entry as the main display) — which is EXACTLY the order ffmpeg's avfoundation
//!   input numbers its `Capture screen N` devices, so idx `N` captures `Capture screen N`.
//! * Linux Wayland: the output layout, the output at the logical origin first. The XDG
//!   ScreenCast portal still decides what is captured (no portal lets an app pre-select a
//!   monitor); the host re-opens the portal picker when the client asks for a different
//!   monitor and reports the one actually chosen via [`index_of_rect`].
//!
//! Nothing here is tested on macOS or Wayland hardware yet (2026-11-14).

/// One host monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
	/// Human-readable name (macOS: `NSScreen.localizedName`; Wayland: the output description
	/// or connector name). EMPTY when the platform gave none (macOS before AppKit's name
	/// cache filled, a Wayland output without name / description / make): the app labels it
	/// in the UI language (`builtin` -> "built-in display", else "display N").
	pub name: String,
	/// The machine's own panel (macOS `CGDisplayIsBuiltin`; Wayland: an eDP / LVDS / DSI
	/// connector) — only used to label a nameless monitor.
	pub builtin: bool,
	/// Origin in the global layout: macOS = CoreGraphics global display space (points, main
	/// display at 0,0); Wayland = the compositor's logical space (xdg-output).
	pub x: i32,
	pub y: i32,
	/// Size in that same layout space (points / logical pixels) — for pointer mapping.
	pub logical_w: u32,
	pub logical_h: u32,
	/// Pixel size of the current mode — what a capture of this monitor produces.
	pub width: u32,
	pub height: u32,
	pub primary: bool,
	/// Current refresh rate (Hz), 0 = unknown.
	pub refresh_hz: u32,
	/// Distinct available `(width, height)` pixel modes, largest area first (may be empty).
	pub modes: Vec<(u32, u32)>,
	/// Platform id: the `CGDirectDisplayID` on macOS, the `wl_output` global name on Wayland.
	pub id: u32,
}

/// The host's monitors, primary at index 0 (see the module docs for the index contract).
/// Empty on Windows, on Linux/X11 (the app enumerates X11 itself via xrandr) and on failure.
pub fn list() -> Vec<Monitor> {
	#[cfg(target_os = "macos")]
	{
		macos::list()
	}
	#[cfg(target_os = "linux")]
	{
		if crate::capture::is_wayland() {
			linux::wayland_list()
		} else {
			Vec::new()
		}
	}
	#[cfg(not(any(target_os = "macos", target_os = "linux")))]
	{
		Vec::new()
	}
}

/// How many monitors [`list`] would return, without names / modes (macOS: the active
/// display count; elsewhere the full list's length).
pub fn count() -> usize {
	#[cfg(target_os = "macos")]
	{
		core_graphics::display::CGDisplay::active_displays().map(|d| d.len()).unwrap_or(0)
	}
	#[cfg(not(target_os = "macos"))]
	{
		list().len()
	}
}

/// The platform id ([`Monitor::id`]) of the monitor at `idx` in [`list`], without the full
/// enumeration (names, modes) — the input path resolves the streamed monitor with it. macOS
/// only (the `CGDirectDisplayID`); `None` elsewhere or when `idx` is out of range.
pub fn platform_id(idx: usize) -> Option<u32> {
	#[cfg(target_os = "macos")]
	{
		core_graphics::display::CGDisplay::active_displays().ok()?.get(idx).copied()
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = idx;
		None
	}
}

/// Start watching for monitor changes (plug / unplug / mode / arrangement / primary). Calls
/// `on_change` from a background (or, on macOS, the main) thread for every change; several
/// calls per physical change are normal — callers debounce and re-read [`list`].
///
/// Never blocks: the source is set up asynchronously (macOS on the main queue, Linux on its
/// own thread), so the return value is only whether it is ALREADY live — usually `false` on
/// the first call. [`watching`] tells whether the event source is live right now (it arms a
/// moment later and can drop out — a lost Wayland/X11 connection), so callers poll while it
/// is not. Calling again after it dropped out starts it again (Linux); while it runs or is
/// starting, a repeat call just returns [`watching`]. Windows: always `false` (the app has
/// its own `WM_DISPLAYCHANGE` listener).
pub fn watch(on_change: Box<dyn Fn() + Send + Sync + 'static>) -> bool {
	#[cfg(target_os = "macos")]
	{
		macos::watch(on_change)
	}
	#[cfg(target_os = "linux")]
	{
		linux::watch(on_change)
	}
	#[cfg(not(any(target_os = "macos", target_os = "linux")))]
	{
		let _ = on_change;
		false
	}
}

/// Whether the [`watch`] event source is currently live.
pub fn watching() -> bool {
	WATCHING.load(std::sync::atomic::Ordering::SeqCst)
}

#[allow(dead_code)]
static WATCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[allow(dead_code)]
fn set_watching(on: bool) {
	WATCHING.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Index in `monitors` of the monitor whose layout rectangle is `(x, y, w, h)` — how the
/// Wayland host learns which output the portal actually streams (the portal reports the
/// stream's position + size in the same logical space). Exact origin match wins; otherwise
/// the monitor with the largest overlap. `None` when nothing overlaps.
pub fn index_of_rect(monitors: &[Monitor], x: i32, y: i32, w: i32, h: i32) -> Option<usize> {
	if let Some(i) = monitors.iter().position(|m| m.x == x && m.y == y) {
		return Some(i);
	}
	let overlap = |m: &Monitor| -> i64 {
		let ox = (x + w).min(m.x + m.logical_w as i32) - x.max(m.x);
		let oy = (y + h).min(m.y + m.logical_h as i32) - y.max(m.y);
		if ox > 0 && oy > 0 {
			ox as i64 * oy as i64
		} else {
			0
		}
	};
	monitors
		.iter()
		.enumerate()
		.map(|(i, m)| (i, overlap(m)))
		.filter(|(_, a)| *a > 0)
		.max_by_key(|(_, a)| *a)
		.map(|(i, _)| i)
}

/// Distinct `(w, h)` pairs, zero sizes dropped, largest area first, capped at 24 (the list
/// travels in `StreamCaps`; the same cap as the Windows enumeration).
pub fn normalize_modes(raw: impl IntoIterator<Item = (u32, u32)>) -> Vec<(u32, u32)> {
	let mut modes: Vec<(u32, u32)> = Vec::new();
	for wh in raw {
		if wh.0 == 0 || wh.1 == 0 || modes.contains(&wh) {
			continue;
		}
		modes.push(wh);
	}
	modes.sort_by(|a, b| (b.0 as u64 * b.1 as u64).cmp(&(a.0 as u64 * a.1 as u64)));
	modes.truncate(24);
	modes
}

/// Order monitors primary first, then top-to-bottom / left-to-right (a stable order matters:
/// the index is what travels in `display_idx`). Marks the monitor at the layout origin
/// primary when none is (Wayland has no primary concept), else the first one.
pub fn order_primary_first(mut monitors: Vec<Monitor>) -> Vec<Monitor> {
	if !monitors.is_empty() && !monitors.iter().any(|m| m.primary) {
		let i = monitors.iter().position(|m| m.x == 0 && m.y == 0).unwrap_or(0);
		monitors[i].primary = true;
	}
	monitors.sort_by_key(|m| (!m.primary, m.y, m.x));
	monitors
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
	use super::*;

	fn mon(x: i32, y: i32, w: u32, h: u32) -> Monitor {
		Monitor {
			name: format!("{x},{y}"),
			x,
			y,
			logical_w: w,
			logical_h: h,
			width: w,
			height: h,
			primary: false,
			refresh_hz: 0,
			modes: Vec::new(),
			id: 0,
			builtin: false,
		}
	}

	#[test]
	fn index_of_rect_prefers_exact_origin() {
		let m = vec![mon(0, 0, 1920, 1080), mon(1920, 0, 2560, 1440)];
		assert_eq!(index_of_rect(&m, 1920, 0, 2560, 1440), Some(1));
		assert_eq!(index_of_rect(&m, 0, 0, 1920, 1080), Some(0));
	}

	#[test]
	fn index_of_rect_falls_back_to_largest_overlap() {
		let m = vec![mon(0, 0, 1920, 1080), mon(1920, 0, 2560, 1440)];
		// A rect shifted a little (scaled outputs round differently) still finds its monitor.
		assert_eq!(index_of_rect(&m, 1921, 2, 2558, 1438), Some(1));
		assert_eq!(index_of_rect(&m, 5000, 5000, 10, 10), None);
	}

	#[test]
	fn modes_are_deduped_sorted_and_capped() {
		let raw = vec![(1280, 720), (0, 0), (3840, 2160), (1280, 720), (1920, 1080)];
		assert_eq!(normalize_modes(raw), vec![(3840, 2160), (1920, 1080), (1280, 720)]);
		let many = (1..100u32).map(|i| (i * 10, i * 10));
		assert_eq!(normalize_modes(many).len(), 24);
	}

	#[test]
	fn origin_monitor_becomes_primary_and_leads() {
		let m = order_primary_first(vec![mon(1920, 0, 2560, 1440), mon(0, 0, 1920, 1080)]);
		assert!(m[0].primary);
		assert_eq!((m[0].x, m[0].y), (0, 0));
		assert!(!m[1].primary);
	}

	#[test]
	fn explicit_primary_is_kept() {
		let mut b = mon(1920, 0, 2560, 1440);
		b.primary = true;
		let m = order_primary_first(vec![mon(0, 0, 1920, 1080), b, mon(-1280, 0, 1280, 1024)]);
		assert_eq!(m[0].x, 1920);
		// Rest ordered by (y, x): the left monitor before the origin one.
		assert_eq!(m[1].x, -1280);
		assert_eq!(m[2].x, 0);
		assert_eq!(m.iter().filter(|m| m.primary).count(), 1);
	}
}
