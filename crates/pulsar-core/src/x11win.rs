//! X11 top-level window enumeration — the Linux side of the per-window capture target
//! (same-host co-op). Mirrors what `pulsar-capture`'s `window.rs` does on Windows with
//! `EnumWindows`:
//!
//! * [`list_windows`] — the host's user-facing top-level windows (`_NET_CLIENT_LIST`
//!   from the EWMH window manager, titled, mapped, not docks/desktops, not Pulsar's
//!   own), so a client can pick one as its capture target;
//! * [`find_window_for_pids`] — the top-level window owned (`_NET_WM_PID`) by a launched
//!   game or one of its child processes (launchers re-parent the real game);
//! * [`window_size`] — the window's current size (the capture plan's frame size).
//!
//! The ids are X window ids (XIDs) as `i64`: the same token the capture path
//! (`x11grab -window_id`) and `pulsar_core::input::WindowInput` take. Only an X server
//! has them: a native Wayland window has no id another client can name, so on a pure
//! Wayland session every function returns empty / `None`. XWayland windows do appear
//! (they are X windows), but capture on a Wayland session goes through the portal,
//! which the host does not narrow to a window, so the caller only uses these on X11.
//!
//! The pure parts (`/proc/<pid>/stat` parent pid, descendant walk) are compiled on every
//! OS so their tests run everywhere.

/// One enumerated window: `(xid, title)`.
pub type WindowDesc = (i64, String);

/// The parent pid from a `/proc/<pid>/stat` line. The command name (field 2) is in
/// parentheses and may itself contain spaces or `)`, so parse after the LAST `)`.
pub fn parse_stat_ppid(stat: &str) -> Option<u32> {
	let rest = &stat[stat.rfind(')')? + 1..];
	let mut it = rest.split_whitespace();
	let _state = it.next()?;
	it.next()?.parse().ok()
}

/// Every descendant of `root` in a `(pid, ppid)` table (children, grandchildren, …),
/// not including `root` itself.
pub fn descendants(table: &[(u32, u32)], root: u32) -> Vec<u32> {
	let mut out = Vec::new();
	let mut frontier = vec![root];
	while let Some(p) = frontier.pop() {
		for &(pid, ppid) in table {
			if ppid == p && pid != root && !out.contains(&pid) {
				out.push(pid);
				frontier.push(pid);
			}
		}
	}
	out
}

/// `pid` and all its live descendants (from `/proc`).
#[cfg(target_os = "linux")]
pub fn pid_tree(pid: u32) -> Vec<u32> {
	let table: Vec<(u32, u32)> = std::fs::read_dir("/proc")
		.map(|rd| {
			rd.filter_map(|e| e.ok())
				.filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
				.filter_map(|p| {
					let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
					Some((p, parse_stat_ppid(&stat)?))
				})
				.collect()
		})
		.unwrap_or_default();
	let mut out = vec![pid];
	out.extend(descendants(&table, pid));
	out
}

#[cfg(target_os = "linux")]
mod imp {
	use super::WindowDesc;
	use x11rb::connection::Connection;
	use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState, Window};
	use x11rb::rust_connection::RustConnection;

	struct Atoms {
		client_list: u32,
		wm_pid: u32,
		wm_name: u32,
		utf8: u32,
		wm_type: u32,
		type_dock: u32,
		type_desktop: u32,
	}

	fn atom(conn: &RustConnection, name: &[u8]) -> u32 {
		conn.intern_atom(false, name)
			.ok()
			.and_then(|c| c.reply().ok())
			.map_or(0, |r| r.atom)
	}

	fn connect() -> Option<(RustConnection, Window, Atoms)> {
		let (conn, screen) = x11rb::connect(None).ok()?;
		let root = conn.setup().roots.get(screen)?.root;
		let atoms = Atoms {
			client_list: atom(&conn, b"_NET_CLIENT_LIST"),
			wm_pid: atom(&conn, b"_NET_WM_PID"),
			wm_name: atom(&conn, b"_NET_WM_NAME"),
			utf8: atom(&conn, b"UTF8_STRING"),
			wm_type: atom(&conn, b"_NET_WM_WINDOW_TYPE"),
			type_dock: atom(&conn, b"_NET_WM_WINDOW_TYPE_DOCK"),
			type_desktop: atom(&conn, b"_NET_WM_WINDOW_TYPE_DESKTOP"),
		};
		Some((conn, root, atoms))
	}

	fn prop32(conn: &RustConnection, w: Window, prop: u32, ty: impl Into<u32>, max: u32) -> Vec<u32> {
		if prop == 0 {
			return Vec::new();
		}
		conn.get_property(false, w, prop, ty.into(), 0, max)
			.ok()
			.and_then(|c| c.reply().ok())
			.and_then(|r| r.value32().map(|v| v.collect()))
			.unwrap_or_default()
	}

	fn clients(conn: &RustConnection, root: Window, a: &Atoms) -> Vec<Window> {
		prop32(conn, root, a.client_list, AtomEnum::WINDOW, 4096)
	}

	fn pid_of(conn: &RustConnection, w: Window, a: &Atoms) -> Option<u32> {
		prop32(conn, w, a.wm_pid, AtomEnum::CARDINAL, 1).first().copied()
	}

	fn title_of(conn: &RustConnection, w: Window, a: &Atoms) -> Option<String> {
		let read = |prop: u32, ty: u32| {
			(prop != 0 && ty != 0)
				.then(|| conn.get_property(false, w, prop, ty, 0, 1024).ok()?.reply().ok())
				.flatten()
				.map(|r| String::from_utf8_lossy(&r.value).trim().to_string())
				.filter(|t| !t.is_empty())
		};
		read(a.wm_name, a.utf8).or_else(|| read(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
	}

	/// Mapped (viewable — a minimized window is unmapped and captures black), non-zero
	/// size, and not a dock/desktop window.
	fn usable(conn: &RustConnection, w: Window, a: &Atoms) -> bool {
		let viewable = conn
			.get_window_attributes(w)
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some_and(|r| r.map_state == MapState::VIEWABLE);
		if !viewable {
			return false;
		}
		let sized = conn
			.get_geometry(w)
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some_and(|g| g.width > 1 && g.height > 1);
		if !sized {
			return false;
		}
		let types = prop32(conn, w, a.wm_type, AtomEnum::ATOM, 16);
		!types.iter().any(|&t| t != 0 && (t == a.type_dock || t == a.type_desktop))
	}

	pub fn list_windows() -> Vec<WindowDesc> {
		let Some((conn, root, a)) = connect() else {
			return Vec::new();
		};
		let me = std::process::id();
		clients(&conn, root, &a)
			.into_iter()
			.filter(|&w| pid_of(&conn, w, &a) != Some(me))
			.filter(|&w| usable(&conn, w, &a))
			.filter_map(|w| Some((w as i64, title_of(&conn, w, &a)?)))
			.collect()
	}

	pub fn find_window_for_pids(pids: &[u32]) -> Option<i64> {
		let (conn, root, a) = connect()?;
		// _NET_CLIENT_LIST is in mapping order: the newest window last. A game that
		// opens a splash and then its main window ends up on the latest one.
		clients(&conn, root, &a)
			.into_iter()
			.rev()
			.filter(|&w| pid_of(&conn, w, &a).is_some_and(|p| pids.contains(&p)))
			.find(|&w| usable(&conn, w, &a) && title_of(&conn, w, &a).is_some())
			.map(|w| w as i64)
	}

	pub fn window_size(id: i64) -> Option<(u32, u32)> {
		let w = u32::try_from(id).ok().filter(|&w| w != 0)?;
		let (conn, _) = x11rb::connect(None).ok()?;
		let g = conn.get_geometry(w).ok()?.reply().ok()?;
		Some((g.width as u32, g.height as u32))
	}
}

/// The host's user-facing top-level X windows (see the module docs). Empty without an
/// X server or an EWMH window manager.
#[cfg(target_os = "linux")]
pub fn list_windows() -> Vec<WindowDesc> {
	imp::list_windows()
}

/// The newest usable top-level window owned by any pid in `pids` — one attempt; the
/// caller polls while the game opens its window.
#[cfg(target_os = "linux")]
pub fn find_window_for_pids(pids: &[u32]) -> Option<i64> {
	imp::find_window_for_pids(pids)
}

/// The window's current size in pixels, `None` if it is gone.
#[cfg(target_os = "linux")]
pub fn window_size(id: i64) -> Option<(u32, u32)> {
	imp::window_size(id)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stat_ppid_survives_odd_command_names() {
		assert_eq!(parse_stat_ppid("1234 (bash) S 1 1234 1234 0 -1"), Some(1));
		assert_eq!(parse_stat_ppid("77 (my game (x) ) R 42 77 77"), Some(42));
		assert_eq!(parse_stat_ppid("garbage"), None);
	}

	#[test]
	fn descendants_walks_the_whole_tree() {
		// 10 → 11 → 12, 10 → 13, 20 unrelated, 14 child of 20.
		let t = [(10, 1), (11, 10), (12, 11), (13, 10), (20, 1), (14, 20)];
		let mut d = descendants(&t, 10);
		d.sort();
		assert_eq!(d, vec![11, 12, 13]);
		assert!(descendants(&t, 12).is_empty());
	}
}
