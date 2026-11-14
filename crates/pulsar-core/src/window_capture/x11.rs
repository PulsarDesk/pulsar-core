//! Linux X11 window listing for per-window capture (pure-Rust `x11rb`, no libX11 link).
//!
//! The list is the window manager's EWMH `_NET_CLIENT_LIST` (the client windows a taskbar
//! shows — not the WM's frame windows), filtered like the Windows picker: mapped (viewable)
//! only, not minimized (`_NET_WM_STATE_HIDDEN`), not a skip-taskbar / transient (dialog) /
//! non-normal-type window (docks, menus, splash screens, desktops), titled, larger than
//! 1×1, and not one of this process's own windows. A WM without EWMH falls back to the
//! root's viewable children that carry a title.
//!
//! The id is what `x11grab -window_id` / `ximagesrc xid=` take. Note: without a compositing
//! manager, the parts of the window covered by OTHER windows are undefined in a grab (core
//! X has no per-window backing pixmap then); with one (KWin, Mutter, picom, xfwm4's
//! compositor) the window is redirected and grabs correctly even when covered.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState, Window, WindowClass};
use x11rb::rust_connection::RustConnection;

use super::HostWindow;

struct Atoms {
	client_list: u32,
	wm_name: u32,
	utf8: u32,
	wm_pid: u32,
	wm_type: u32,
	type_normal: u32,
	wm_state: u32,
	skip_taskbar: u32,
	hidden: u32,
}

fn atom(conn: &RustConnection, name: &str) -> u32 {
	conn.intern_atom(false, name.as_bytes())
		.ok()
		.and_then(|c| c.reply().ok())
		.map(|r| r.atom)
		.unwrap_or(0)
}

fn atoms(conn: &RustConnection) -> Atoms {
	Atoms {
		client_list: atom(conn, "_NET_CLIENT_LIST"),
		wm_name: atom(conn, "_NET_WM_NAME"),
		utf8: atom(conn, "UTF8_STRING"),
		wm_pid: atom(conn, "_NET_WM_PID"),
		wm_type: atom(conn, "_NET_WM_WINDOW_TYPE"),
		type_normal: atom(conn, "_NET_WM_WINDOW_TYPE_NORMAL"),
		wm_state: atom(conn, "_NET_WM_STATE"),
		skip_taskbar: atom(conn, "_NET_WM_STATE_SKIP_TASKBAR"),
		hidden: atom(conn, "_NET_WM_STATE_HIDDEN"),
	}
}

/// A 32-bit-format property as a list (empty when unset / of another format).
fn prop32(conn: &RustConnection, win: Window, prop: u32, ty: impl Into<u32>) -> Vec<u32> {
	if prop == 0 {
		return Vec::new();
	}
	conn.get_property(false, win, prop, ty.into(), 0, 4096)
		.ok()
		.and_then(|c| c.reply().ok())
		.and_then(|r| r.value32().map(|v| v.collect()))
		.unwrap_or_default()
}

/// An 8-bit string property (`None` when unset).
fn prop_str(conn: &RustConnection, win: Window, prop: u32, ty: impl Into<u32>) -> Option<String> {
	if prop == 0 {
		return None;
	}
	let r = conn.get_property(false, win, prop, ty.into(), 0, 1024).ok()?.reply().ok()?;
	if r.format != 8 || r.value.is_empty() {
		return None;
	}
	Some(String::from_utf8_lossy(&r.value).into_owned())
}

/// The window's caption: `_NET_WM_NAME` (UTF-8), else ICCCM `WM_NAME`.
fn title(conn: &RustConnection, a: &Atoms, win: Window) -> Option<String> {
	prop_str(conn, win, a.wm_name, a.utf8)
		.or_else(|| prop_str(conn, win, AtomEnum::WM_NAME.into(), AtomEnum::ANY))
		.map(|t| t.trim().to_string())
		.filter(|t| !t.is_empty())
}

/// One candidate, or `None` when it fails the filter (see the module docs).
fn describe(conn: &RustConnection, a: &Atoms, win: Window, ewmh: bool) -> Option<HostWindow> {
	let attrs = conn.get_window_attributes(win).ok()?.reply().ok()?;
	if attrs.map_state != MapState::VIEWABLE || attrs.class == WindowClass::INPUT_ONLY {
		return None;
	}
	if !ewmh && attrs.override_redirect {
		return None;
	}
	let state = prop32(conn, win, a.wm_state, AtomEnum::ATOM);
	if state.iter().any(|s| *s != 0 && (*s == a.skip_taskbar || *s == a.hidden)) {
		return None;
	}
	let types = prop32(conn, win, a.wm_type, AtomEnum::ATOM);
	if !types.is_empty() && !types.contains(&a.type_normal) {
		return None;
	}
	// Dialogs / palettes of another window (the Windows picker skips owned windows too).
	if !prop32(conn, win, AtomEnum::WM_TRANSIENT_FOR.into(), AtomEnum::WINDOW).is_empty() {
		return None;
	}
	let pid = prop32(conn, win, a.wm_pid, AtomEnum::CARDINAL).first().copied().unwrap_or(0);
	if pid == std::process::id() {
		return None;
	}
	let title = title(conn, a, win)?;
	let geo = conn.get_geometry(win).ok()?.reply().ok()?;
	if geo.width <= 1 || geo.height <= 1 {
		return None;
	}
	Some(HostWindow { id: win as u64, title, pid, width: geo.width as u32, height: geo.height as u32 })
}

pub(super) fn list() -> Vec<HostWindow> {
	let Ok((conn, screen)) = x11rb::connect(None) else {
		tracing::debug!("window list: no X11 connection");
		return Vec::new();
	};
	let Some(root) = conn.setup().roots.get(screen).map(|s| s.root) else { return Vec::new() };
	let a = atoms(&conn);
	let clients = prop32(&conn, root, a.client_list, AtomEnum::WINDOW);
	if !clients.is_empty() {
		return clients.into_iter().filter_map(|w| describe(&conn, &a, w, true)).collect();
	}
	// No EWMH window manager: the root's direct children (top-level windows).
	let children = conn
		.query_tree(root)
		.ok()
		.and_then(|c| c.reply().ok())
		.map(|t| t.children)
		.unwrap_or_default();
	children.into_iter().filter_map(|w| describe(&conn, &a, w, false)).collect()
}

pub(super) fn size(id: u64) -> Option<(u32, u32)> {
	let (conn, _) = x11rb::connect(None).ok()?;
	let win = u32::try_from(id).ok()?;
	let geo = conn.get_geometry(win).ok()?.reply().ok()?;
	Some((geo.width as u32, geo.height as u32))
}
