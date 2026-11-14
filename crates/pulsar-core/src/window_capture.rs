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

/// The in-process window sources (macOS [`WindowStream`]) of the host's sessions, keyed by
/// session id: the host app starts one per stream and must stop it on every re-stream and
/// teardown ([`stop_for`]). One per session at most.
#[cfg(target_os = "macos")]
static SESSIONS: std::sync::Mutex<Vec<(u64, WindowStream)>> = std::sync::Mutex::new(Vec::new());

/// Start capturing window `id` for `session` (replacing that session's previous one) at
/// exactly `width`×`height` and `fps`, and return the url its raw NV12 frames are served
/// on — the `url` of [`WindowInput::RawNv12`](crate::pipeline::WindowInput::RawNv12).
/// macOS only (ScreenCaptureKit); every other host has no in-process window source and
/// gets an error (X11 grabs by id in ffmpeg/gst, Wayland goes through the portal).
pub fn serve_for(session: u64, id: u64, width: u32, height: u32, fps: u32) -> anyhow::Result<String> {
	#[cfg(target_os = "macos")]
	{
		stop_for(session);
		let stream = WindowStream::start(id, width, height, fps)?;
		let url = stream.url().to_string();
		SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).push((session, stream));
		Ok(url)
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = (session, id, width, height, fps);
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
