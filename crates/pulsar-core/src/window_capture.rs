//! Host **per-window** capture sources off Windows (the "Pencere" pick / the window of a game
//! the host launched). Windows captures one window natively through WGC in the desktop app's
//! `pulsar-capture`; this module is the Linux + macOS side of the same feature:
//!
//! | Host | Listing | Window id on the wire | Capture |
//! | --- | --- | --- | --- |
//! | Linux X11 | EWMH `_NET_CLIENT_LIST` + titles (`x11.rs`, pure-Rust `x11rb`) | X window id | ffmpeg `x11grab -window_id` / libav / gst `ximagesrc xid=` (arg builders in `pipeline`) |
//! | Linux Wayland | none possible (see [`WindowInfo::PORTAL_PICK`]) | `PORTAL_PICK` | ScreenCast portal, source type WINDOW (`crate::capture::start_with`) |
//! | macOS | ScreenCaptureKit `SCShareableContent` (`mac.rs`) | `CGWindowID` | ScreenCaptureKit desktop-independent window → NV12 over loopback TCP → ffmpeg ([`WindowStream`]) |
//!
//! Everything here is a no-op (empty / `None`) on other targets, so callers need no `cfg`.
//!
//! [`WindowInfo::PORTAL_PICK`]: crate::service::WindowInfo::PORTAL_PICK

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "macos")]
pub use mac::WindowStream;

/// One capture-able top-level window of this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostWindow {
	/// The per-OS window id (X window id / `CGWindowID`), see the module table.
	pub id: u64,
	/// Caption shown in the client's picker (macOS: the app name when the window has none).
	pub title: String,
	/// Owning process (0 when the window does not say, e.g. an X client without
	/// `_NET_WM_PID`).
	pub pid: u32,
	/// Current size in PIXELS (macOS: points × the display's backing scale).
	pub width: u32,
	pub height: u32,
}

/// A window's rectangle on the desktop, in the space the host's pointer injector works in:
/// X11 root-window pixels, macOS global display POINTS (top-left origin, as `CGEvent` and
/// `CGWindowListCopyWindowInfo` use). The client area only on X11 (what `x11grab -window_id`
/// grabs); the whole window incl. its title bar on macOS (what ScreenCaptureKit captures).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowRect {
	pub x: f64,
	pub y: f64,
	pub w: f64,
	pub h: f64,
}

impl WindowRect {
	fn centre(&self) -> (f64, f64) {
		(self.x + self.w / 2.0, self.y + self.h / 2.0)
	}

	fn contains(&self, (x, y): (f64, f64)) -> bool {
		x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
	}
}

/// Where a window was before [`fit`] resized it ([`restore`] puts it back).
#[derive(Clone, Debug, PartialEq)]
pub struct FitPrev {
	#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
	id: u64,
	#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
	rect: WindowRect,
	/// X11: it was maximized vertically / horizontally (re-applied on restore).
	#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
	maximized: (bool, bool),
}

/// Which per-window source this host has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
	/// Linux X11 session: windows are listed and grabbed by id.
	X11,
	/// Linux Wayland session: the person at the host picks the window in the portal dialog.
	Portal,
	/// macOS 12.3+: ScreenCaptureKit.
	ScreenCaptureKit,
	/// No per-window source (other targets, or Linux with neither X11 nor Wayland).
	None,
}

/// The per-window source of this session (Linux decides per session type at runtime).
pub fn backend() -> Backend {
	#[cfg(target_os = "linux")]
	{
		if crate::capture::is_wayland() {
			return Backend::Portal;
		}
		if std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()) {
			return Backend::X11;
		}
		Backend::None
	}
	#[cfg(target_os = "macos")]
	{
		if mac::available() {
			Backend::ScreenCaptureKit
		} else {
			Backend::None
		}
	}
	#[cfg(not(any(target_os = "linux", target_os = "macos")))]
	{
		Backend::None
	}
}

/// The host's visible, titled, user-facing top-level windows (not this process's own).
/// Empty on Wayland (the portal picks), on other targets, or when the display server /
/// ScreenCaptureKit can't be reached (macOS without the Screen Recording grant).
pub fn list() -> Vec<HostWindow> {
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::list(),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::list(),
		_ => Vec::new(),
	}
}

/// Current pixel size of window `id` (`None` when it is gone / no backend).
pub fn size(id: u64) -> Option<(u32, u32)> {
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::size(id),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::size(id),
		_ => {
			let _ = id;
			None
		}
	}
}

/// Window `id`'s current rectangle on the desktop (see [`WindowRect`]); `None` when it is
/// gone / no backend. Cheap (no ScreenCaptureKit query): the host's input path polls it.
pub fn rect(id: u64) -> Option<WindowRect> {
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::placement(id).map(|(r, _)| r),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::rect(id),
		_ => {
			let _ = id;
			None
		}
	}
}

/// Linux X11: window `id` as the 0..1 region of the screen the absolute uinput pointer
/// spans (the X server maps it over the whole root window, every monitor), for
/// `DesktopInput::set_region`: the client's pointer on the window stream then lands in
/// that window. `None` = gone / not X11.
#[cfg(target_os = "linux")]
pub fn pointer_region(id: u64) -> Option<crate::capture::PointerRegion> {
	if backend() != Backend::X11 {
		return None;
	}
	let (r, desk) = x11::placement(id)?;
	let (x, y, w, h) = region_in_desktop(r, desk)?;
	Some(crate::capture::PointerRegion { x, y, w, h })
}

/// Bring window `id` to the front and give it the keyboard focus, so a window stream's
/// input goes to it (X11: EWMH `_NET_ACTIVE_WINDOW`; macOS: activate the owning app and
/// raise the window through Accessibility when granted). Best effort; no-op elsewhere.
pub fn raise(id: u64) {
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::raise(id),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::raise(id),
		_ => {
			let _ = id;
		}
	}
}

/// Resize window `id` to a split pane's `pane_w`×`pane_h` aspect, as large as fits the
/// monitor's work area (never larger than the pane), and return where it was. `None` when
/// nothing changed (gone, full-screen, no backend; macOS: no Accessibility grant). The
/// counterpart of the Windows `display_mode::fit_window`. Blocks for up to ~0.5 s while
/// it waits for the window manager to apply the new size.
pub fn fit(id: u64, pane_w: u32, pane_h: u32) -> Option<FitPrev> {
	if pane_w == 0 || pane_h == 0 {
		return None;
	}
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::fit(id, pane_w, pane_h),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::fit(id, pane_w, pane_h),
		_ => {
			let _ = id;
			None
		}
	}
}

/// Put a window [`fit`] resized back where it was (no-op when it is gone).
pub fn restore(prev: FitPrev) {
	match backend() {
		#[cfg(target_os = "linux")]
		Backend::X11 => x11::restore(&prev),
		#[cfg(target_os = "macos")]
		Backend::ScreenCaptureKit => mac::restore(&prev),
		_ => {
			let _ = prev;
		}
	}
}

/// `r` as fractions of a `desk`-sized layout: the pixel centres of its first and last
/// pixel, so 1.0 lands on the window's last pixel (the same rule as the Wayland monitor
/// region). A window partly off screen keeps its true region — the injector clamps the
/// final position into the layout. `None` for an empty layout / window.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn region_in_desktop(r: WindowRect, (dw, dh): (f64, f64)) -> Option<(f64, f64, f64, f64)> {
	if dw < 1.0 || dh < 1.0 || r.w < 1.0 || r.h < 1.0 {
		return None;
	}
	Some(((r.x + 0.5) / dw, (r.y + 0.5) / dh, (r.w - 1.0) / dw, (r.h - 1.0) / dh))
}

/// Largest `w × h` with the pane's aspect that fits `max_w × max_h` and is no larger than the
/// pane (even, at least 64) — the same rule as the Windows window fit.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn fit_client(pane_w: u32, pane_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
	let scale = (max_w as f64 / pane_w.max(1) as f64).min(max_h as f64 / pane_h.max(1) as f64).min(1.0);
	let w = ((pane_w as f64 * scale) as u32).max(64) & !1;
	let h = ((pane_h as f64 * scale) as u32).max(64) & !1;
	(w, h)
}

/// The start of a `len`-long span placed as close to `pos` as fits `[lo, lo + room)` (the
/// window stays on its monitor's work area; a span larger than the room starts at `lo`).
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn place(pos: f64, len: f64, lo: f64, room: f64) -> f64 {
	pos.min(lo + room - len).max(lo)
}

/// The work area (of `areas`, e.g. monitors) that holds `r`'s centre, else the first one.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn area_of(r: &WindowRect, areas: &[WindowRect]) -> Option<WindowRect> {
	areas.iter().find(|a| a.contains(r.centre())).or(areas.first()).copied()
}

/// How far apart two frames are (sum of the position and size differences): macOS matches
/// an Accessibility window to a window-server window by frame (AX exposes no window id).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn frame_distance(a: WindowRect, b: WindowRect) -> f64 {
	(a.x - b.x).abs() + (a.y - b.y).abs() + (a.w - b.w).abs() + (a.h - b.h).abs()
}

/// The overlap of two rectangles (`None` when they do not overlap).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn intersect(a: WindowRect, b: WindowRect) -> Option<WindowRect> {
	let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
	let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
	(x1 > x0 && y1 > y0).then_some(WindowRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

/// The in-process window sources (macOS [`WindowStream`]) of the host's sessions, keyed by
/// session id: the host app starts one per stream and must stop it on every re-stream and
/// teardown ([`stop_for`]). One per session at most.
#[cfg(target_os = "macos")]
static SESSIONS: std::sync::Mutex<Vec<(u64, WindowStream)>> = std::sync::Mutex::new(Vec::new());

/// Start capturing window `id` for `session` (replacing that session's previous one), its
/// own aspect fitted into the `max` box (never upscaled, even — `pipeline::fit_within`), at
/// `fps`. Returns the url its raw NV12 frames are served on — the `url` of
/// [`WindowInput::RawNv12`](crate::pipeline::WindowInput::RawNv12) — and the frame size,
/// both from ONE ScreenCaptureKit query. Blocks for up to ~10 s (the shareable-content
/// query and the start each wait up to 5 s, e.g. behind the Screen Recording prompt): call
/// it off the async runtime. macOS only (ScreenCaptureKit); every other host has no
/// in-process window source and gets an error (X11 grabs by id in ffmpeg/gst, Wayland goes
/// through the portal).
pub fn serve_for(session: u64, id: u64, max: (u32, u32), fps: u32) -> anyhow::Result<(String, (u32, u32))> {
	#[cfg(target_os = "macos")]
	{
		stop_for(session);
		let stream = WindowStream::start(id, max, fps)?;
		let out = (stream.url().to_string(), stream.size());
		SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).push((session, stream));
		Ok(out)
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = (session, id, max, fps);
		anyhow::bail!("this host has no in-process window source")
	}
}

/// Stop `session`'s in-process window source, if any (no-op elsewhere / when none runs).
pub fn stop_for(session: u64) {
	#[cfg(target_os = "macos")]
	{
		let taken: Vec<_> = {
			let mut g = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let (gone, kept) = std::mem::take(&mut *g).into_iter().partition(|(s, _)| *s == session);
			*g = kept;
			gone
		};
		// Dropped outside the lock: stopping joins the frame server thread.
		drop(taken);
	}
	#[cfg(not(target_os = "macos"))]
	let _ = session;
}

/// `session`'s in-process window source runs (`None` = it has none).
pub fn alive_for(session: u64) -> Option<bool> {
	#[cfg(target_os = "macos")]
	{
		let g = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		g.iter().find(|(s, _)| *s == session).map(|(_, w)| w.is_alive())
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = session;
		None
	}
}

/// The window of a launched program: the largest listed window owned by `pid` **or any of
/// its descendants** (launchers such as Steam start the real game in a child). A single
/// attempt — games open their window seconds after starting, so the caller polls.
/// Always `None` on Wayland (no client can see other programs' windows there).
pub fn find_for_launch(pid: u32) -> Option<u64> {
	if !matches!(backend(), Backend::X11 | Backend::ScreenCaptureKit) {
		return None;
	}
	let mut pids = descendant_pids(pid);
	pids.push(pid);
	largest_owned_by(&list(), &pids)
}

/// The largest window (by area) whose owner is in `pids`; the first one wins a tie.
pub fn largest_owned_by(windows: &[HostWindow], pids: &[u32]) -> Option<u64> {
	let mut best: Option<(u64, u64)> = None;
	for w in windows.iter().filter(|w| w.pid != 0 && pids.contains(&w.pid)) {
		let area = w.width as u64 * w.height as u64;
		if best.is_none_or(|(_, a)| area > a) {
			best = Some((w.id, area));
		}
	}
	best.map(|(id, _)| id)
}

/// Every descendant pid of `root` (not `root` itself), from one process snapshot.
pub fn descendant_pids(root: u32) -> Vec<u32> {
	descendants_from_edges(&process_edges(), root)
}

/// Transitive closure over `(pid, parent)` edges.
fn descendants_from_edges(edges: &[(u32, u32)], root: u32) -> Vec<u32> {
	let mut set: Vec<u32> = Vec::new();
	let mut frontier = vec![root];
	while let Some(p) = frontier.pop() {
		for &(pid, parent) in edges {
			if parent == p && pid != root && !set.contains(&pid) {
				set.push(pid);
				frontier.push(pid);
			}
		}
	}
	set
}

/// `(pid, parent pid)` of every process.
#[cfg(target_os = "linux")]
fn process_edges() -> Vec<(u32, u32)> {
	let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
	dir.filter_map(|e| e.ok())
		.filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
		.filter_map(|pid| {
			let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
			Some((pid, parent_from_stat(&stat)?))
		})
		.collect()
}

/// `(pid, parent pid)` of every process (`proc_listallpids` + `proc_pidinfo`).
#[cfg(target_os = "macos")]
fn process_edges() -> Vec<(u32, u32)> {
	mac::process_edges()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_edges() -> Vec<(u32, u32)> {
	Vec::new()
}

/// The parent pid in a `/proc/<pid>/stat` line. The command name (field 2) is in
/// parentheses and may itself contain spaces and `)`, so parse after the LAST `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parent_from_stat(stat: &str) -> Option<u32> {
	let rest = &stat[stat.rfind(')')? + 1..];
	// " S 1234 …": state, then ppid.
	rest.split_whitespace().nth(1)?.parse().ok()
}

/// The caption the picker shows for a window: "Title — App", or the app name alone for an
/// untitled window (macOS apps often leave their main window untitled); `None` when both
/// are empty (such a window is not offered).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn display_title(title: &str, app: &str) -> Option<String> {
	let (t, a) = (title.trim(), app.trim());
	match (t.is_empty(), a.is_empty()) {
		(true, true) => None,
		(true, false) => Some(a.to_string()),
		(false, true) => Some(t.to_string()),
		(false, false) if t == a => Some(t.to_string()),
		(false, false) => Some(format!("{t} — {a}")),
	}
}

/// Copy `rows` rows of `width` bytes from a strided plane into a tightly packed one. Rows
/// or bytes the source lacks (a smaller buffer than configured) stay as they were, so a
/// short frame never reads out of bounds.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn copy_plane(dst: &mut [u8], width: usize, rows: usize, src: &[u8], src_stride: usize, src_rows: usize, src_width: usize) {
	let n = width.min(src_width).min(src_stride);
	for r in 0..rows.min(src_rows) {
		let (d, s) = (r * width, r * src_stride);
		if d + n > dst.len() || s + n > src.len() {
			break;
		}
		dst[d..d + n].copy_from_slice(&src[s..s + n]);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn r(x: f64, y: f64, w: f64, h: f64) -> WindowRect {
		WindowRect { x, y, w, h }
	}

	#[test]
	fn window_region_in_the_desktop() {
		// A 1001×501 window at (1000, 500) on a 3000×1500 two-monitor root.
		let (x, y, w, h) = region_in_desktop(r(1000.0, 500.0, 1001.0, 501.0), (3000.0, 1500.0)).unwrap();
		assert!((x - 1000.5 / 3000.0).abs() < 1e-12);
		assert!((y - 500.5 / 1500.0).abs() < 1e-12);
		assert!((w - 1000.0 / 3000.0).abs() < 1e-12);
		assert!((h - 500.0 / 1500.0).abs() < 1e-12);
		// 1.0 of the stream lands on the window's last pixel centre.
		assert!(((x + w) * 3000.0 - 2000.5).abs() < 1e-9);
		// Partly off screen: the true region (the injector clamps later).
		let (x, ..) = region_in_desktop(r(-100.0, 0.0, 200.0, 200.0), (1000.0, 1000.0)).unwrap();
		assert!(x < 0.0);
		assert_eq!(region_in_desktop(r(0.0, 0.0, 0.0, 10.0), (1000.0, 1000.0)), None);
		assert_eq!(region_in_desktop(r(0.0, 0.0, 10.0, 10.0), (0.0, 1000.0)), None);
	}

	#[test]
	fn fit_keeps_the_pane_aspect_inside_the_work_area() {
		// A wide top/bottom pane on a 1920×1040 work area: width-bound, aspect kept.
		let (w, h) = fit_client(2560, 700, 1904, 1009);
		assert_eq!(w, 1904);
		assert!((h as i32 - 520).abs() <= 2, "{h}");
		// Never larger than the pane itself.
		assert_eq!(fit_client(800, 600, 1904, 1009), (800, 600));
		// Even sizes, at least 64.
		assert_eq!(fit_client(10, 10, 1000, 1000), (64, 64));
	}

	#[test]
	fn placement_stays_in_the_work_area() {
		assert_eq!(place(100.0, 500.0, 0.0, 1920.0), 100.0);
		// Would overflow the right edge: pulled back in.
		assert_eq!(place(1800.0, 500.0, 0.0, 1920.0), 1420.0);
		// Left of the area (the next monitor's work area starts at 1920).
		assert_eq!(place(1000.0, 500.0, 1920.0, 1920.0), 1920.0);
		// Larger than the room: starts at the area's edge.
		assert_eq!(place(300.0, 3000.0, 0.0, 1920.0), 0.0);
	}

	#[test]
	fn work_area_is_the_one_under_the_window() {
		let mons = [r(0.0, 0.0, 1920.0, 1080.0), r(1920.0, 0.0, 2560.0, 1440.0)];
		assert_eq!(area_of(&r(2000.0, 100.0, 800.0, 600.0), &mons), Some(mons[1]));
		assert_eq!(area_of(&r(100.0, 100.0, 800.0, 600.0), &mons), Some(mons[0]));
		// Centre off every monitor: the first.
		assert_eq!(area_of(&r(-5000.0, 0.0, 10.0, 10.0), &mons), Some(mons[0]));
		assert_eq!(area_of(&r(0.0, 0.0, 10.0, 10.0), &[]), None);
		// The EWMH work area (all monitors minus panels) clipped to one monitor.
		let work = r(0.0, 32.0, 4480.0, 1408.0);
		assert_eq!(intersect(work, mons[0]), Some(r(0.0, 32.0, 1920.0, 1048.0)));
		assert_eq!(intersect(r(0.0, 0.0, 10.0, 10.0), r(20.0, 0.0, 10.0, 10.0)), None);
	}

	#[test]
	fn frame_distance_sums_every_edge() {
		let a = r(10.0, 20.0, 800.0, 600.0);
		assert_eq!(frame_distance(a, a), 0.0);
		assert_eq!(frame_distance(a, r(11.0, 18.0, 800.0, 603.0)), 6.0);
	}

	#[test]
	fn copy_plane_strips_stride_and_never_overreads() {
		// 4×2 picture in a 6-byte-stride buffer.
		let src = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
		let mut dst = [9u8; 8];
		copy_plane(&mut dst, 4, 2, &src, 6, 2, 4);
		assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8]);
		// The source is narrower and shorter than the target: the rest stays untouched.
		let mut dst = [9u8; 8];
		copy_plane(&mut dst, 4, 2, &[1, 2, 3], 3, 1, 3);
		assert_eq!(dst, [1, 2, 3, 9, 9, 9, 9, 9]);
		// A lying stride / row count never reads past the slice.
		let mut dst = [0u8; 8];
		copy_plane(&mut dst, 4, 2, &[1, 2, 3, 4], 6, 2, 4);
		assert_eq!(dst, [1, 2, 3, 4, 0, 0, 0, 0]);
	}

	#[test]
	fn picker_title_falls_back_to_the_app() {
		assert_eq!(display_title("Doc.txt", "TextEdit").as_deref(), Some("Doc.txt — TextEdit"));
		assert_eq!(display_title("  ", "Finder").as_deref(), Some("Finder"));
		assert_eq!(display_title("Steam", "Steam").as_deref(), Some("Steam"));
		assert_eq!(display_title("Untitled", "").as_deref(), Some("Untitled"));
		assert_eq!(display_title("", " "), None);
	}

	fn w(id: u64, pid: u32, width: u32, height: u32) -> HostWindow {
		HostWindow { id, title: format!("w{id}"), pid, width, height }
	}

	#[test]
	fn largest_window_of_the_process_tree_wins() {
		let list = [w(1, 10, 100, 100), w(2, 11, 1920, 1080), w(3, 99, 4000, 4000), w(4, 0, 5000, 5000)];
		// pid 11 (a child) owns the big game window; pid 99 is somebody else; pid 0 = unknown.
		assert_eq!(largest_owned_by(&list, &[10, 11]), Some(2));
		assert_eq!(largest_owned_by(&list, &[10]), Some(1));
		assert_eq!(largest_owned_by(&list, &[42]), None);
		assert_eq!(largest_owned_by(&list, &[0]), None, "unknown owner never matches");
		// Tie: the first listed stays.
		let tie = [w(7, 5, 10, 10), w(8, 5, 10, 10)];
		assert_eq!(largest_owned_by(&tie, &[5]), Some(7));
	}

	#[test]
	fn descendants_are_transitive_and_exclude_the_root() {
		// 1 → 10 → 11 → 12, 1 → 20, 30 unrelated (parent 2)
		let edges = [(10, 1), (11, 10), (12, 11), (20, 1), (30, 2), (1, 0)];
		let mut d = descendants_from_edges(&edges, 10);
		d.sort();
		assert_eq!(d, vec![11, 12]);
		let mut d = descendants_from_edges(&edges, 1);
		d.sort();
		assert_eq!(d, vec![10, 11, 12, 20]);
		assert!(descendants_from_edges(&edges, 12).is_empty());
		// A cycle (bogus snapshot) terminates.
		assert_eq!(descendants_from_edges(&[(2, 3), (3, 2)], 2), vec![3]);
	}

	#[test]
	fn parent_pid_from_proc_stat() {
		assert_eq!(parent_from_stat("1234 (bash) S 1000 1234 1234 0 -1"), Some(1000));
		// A command name with spaces and parentheses.
		assert_eq!(parent_from_stat("77 (Web Content (x)) R 42 77 1 0"), Some(42));
		assert_eq!(parent_from_stat("garbage"), None);
	}
}
