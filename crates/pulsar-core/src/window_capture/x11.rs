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

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
	AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus, MapState,
	StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use super::{FitPrev, HostWindow, WindowRect};

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

/// A connection, its root window and the root's size in pixels (the whole X screen —
/// every monitor — which is what the absolute uinput pointer spans).
fn open() -> Option<(RustConnection, Window, (f64, f64))> {
	let (conn, screen) = x11rb::connect(None).ok()?;
	let s = conn.setup().roots.get(screen)?;
	let (root, desk) = (s.root, (s.width_in_pixels as f64, s.height_in_pixels as f64));
	Some((conn, root, desk))
}

/// The client window's rectangle in root coordinates (reparenting WMs put it inside a
/// frame, so its own x/y are frame-relative: translate them to the root).
fn geometry(conn: &RustConnection, root: Window, win: Window) -> Option<WindowRect> {
	let geo = conn.get_geometry(win).ok()?.reply().ok()?;
	let at = conn.translate_coordinates(win, root, 0, 0).ok()?.reply().ok()?;
	Some(WindowRect { x: at.dst_x as f64, y: at.dst_y as f64, w: geo.width as f64, h: geo.height as f64 })
}

/// Window `id`'s rectangle in root pixels and the root's size.
pub(super) fn placement(id: u64) -> Option<(WindowRect, (f64, f64))> {
	let (conn, root, desk) = open()?;
	let r = geometry(&conn, root, u32::try_from(id).ok()?)?;
	Some((r, desk))
}

/// The window manager announces support for `atom` (EWMH `_NET_SUPPORTED`).
fn supported(conn: &RustConnection, root: Window, atom_id: u32) -> bool {
	atom_id != 0 && prop32(conn, root, atom(conn, "_NET_SUPPORTED"), AtomEnum::ATOM).contains(&atom_id)
}

/// Send an EWMH client message about `win` to the root (where the WM listens).
fn client_message(conn: &RustConnection, root: Window, win: Window, ty: u32, data: [u32; 5]) {
	let ev = ClientMessageEvent::new(32, win, ty, data);
	let _ = conn.send_event(
		false,
		root,
		EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
		ev,
	);
}

/// EWMH "source indication" 2: a pager / tool acting for the user (WMs honour these
/// requests without their focus-stealing prevention).
const SOURCE_PAGER: u32 = 2;

pub(super) fn raise(id: u64) {
	let Some((conn, root, _)) = open() else { return };
	let Ok(win) = u32::try_from(id) else { return };
	let active = atom(&conn, "_NET_ACTIVE_WINDOW");
	if supported(&conn, root, active) {
		client_message(&conn, root, win, active, [SOURCE_PAGER, x11rb::CURRENT_TIME, 0, 0, 0]);
	} else {
		// No EWMH window manager: restack and focus it ourselves.
		let _ = conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
		let _ = conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
	}
	let _ = conn.flush();
}

/// Move + resize the CLIENT window to `r` (root coordinates). EWMH `_NET_MOVERESIZE_WINDOW`
/// with StaticGravity (x/y = the client's own position, the WM adds its frame), else a
/// plain ConfigureWindow (no WM: the window is a direct child of the root).
fn move_resize(conn: &RustConnection, root: Window, win: Window, r: WindowRect) {
	let msg = atom(conn, "_NET_MOVERESIZE_WINDOW");
	let (x, y, w, h) = (r.x.round() as i32, r.y.round() as i32, r.w.round().max(1.0) as u32, r.h.round().max(1.0) as u32);
	if supported(conn, root, msg) {
		const STATIC_GRAVITY: u32 = 10;
		let flags = STATIC_GRAVITY | (0xF << 8) | (SOURCE_PAGER << 12);
		client_message(conn, root, win, msg, [flags, x as u32, y as u32, w, h]);
	} else {
		let _ = conn.configure_window(win, &ConfigureWindowAux::new().x(x).y(y).width(w).height(h));
	}
	let _ = conn.flush();
}

/// Ask the WM to add (`true`) or remove the maximized-vertically / -horizontally states.
fn set_maximized(conn: &RustConnection, root: Window, win: Window, add: bool, (v, h): (bool, bool)) {
	let state = atom(conn, "_NET_WM_STATE");
	if state == 0 || !(v || h) {
		return;
	}
	let pick = |want: bool, name: &str| if want { atom(conn, name) } else { 0 };
	let (av, ah) = (pick(v, "_NET_WM_STATE_MAXIMIZED_VERT"), pick(h, "_NET_WM_STATE_MAXIMIZED_HORZ"));
	client_message(conn, root, win, state, [add as u32, av, ah, SOURCE_PAGER, 0]);
	let _ = conn.flush();
}

/// Wait (up to ~0.5 s) until the WM applied a new size to `win`, so the stream that starts
/// right after the fit already grabs the new geometry.
fn wait_for_size(conn: &RustConnection, root: Window, win: Window, from: WindowRect) {
	let until = Instant::now() + Duration::from_millis(500);
	while Instant::now() < until {
		match geometry(conn, root, win) {
			Some(g) if (g.w, g.h) != (from.w, from.h) => return,
			None => return,
			_ => std::thread::sleep(Duration::from_millis(25)),
		}
	}
}

/// The work areas a window can be fitted into: each RandR monitor clipped to the EWMH
/// work area (panels / docks excluded), else the work area, else the whole root.
fn work_areas(conn: &RustConnection, root: Window, desk: (f64, f64)) -> Vec<WindowRect> {
	let whole = WindowRect { x: 0.0, y: 0.0, w: desk.0, h: desk.1 };
	let current = prop32(conn, root, atom(conn, "_NET_CURRENT_DESKTOP"), AtomEnum::CARDINAL)
		.first()
		.copied()
		.unwrap_or(0) as usize;
	let areas = prop32(conn, root, atom(conn, "_NET_WORKAREA"), AtomEnum::CARDINAL);
	let quad = areas.chunks_exact(4).nth(current).or_else(|| areas.chunks_exact(4).next());
	let work = quad.map_or(whole, |q| WindowRect {
		x: q[0] as i32 as f64,
		y: q[1] as i32 as f64,
		w: q[2] as f64,
		h: q[3] as f64,
	});
	let monitors: Vec<WindowRect> = conn
		.randr_get_monitors(root, true)
		.ok()
		.and_then(|c| c.reply().ok())
		.map(|r| {
			r.monitors
				.iter()
				.map(|m| WindowRect { x: m.x as f64, y: m.y as f64, w: m.width as f64, h: m.height as f64 })
				.collect()
		})
		.unwrap_or_default();
	let clipped: Vec<WindowRect> = monitors.into_iter().filter_map(|m| super::intersect(m, work)).collect();
	if clipped.is_empty() {
		vec![work]
	} else {
		clipped
	}
}

pub(super) fn fit(id: u64, pane_w: u32, pane_h: u32) -> Option<FitPrev> {
	let (conn, root, desk) = open()?;
	let win = u32::try_from(id).ok()?;
	let state = prop32(&conn, win, atom(&conn, "_NET_WM_STATE"), AtomEnum::ATOM);
	let has = |name: &str| {
		let a = atom(&conn, name);
		a != 0 && state.contains(&a)
	};
	if has("_NET_WM_STATE_FULLSCREEN") {
		tracing::info!(id, "window-fit: the window is full-screen, left as it is");
		return None;
	}
	let maximized = (has("_NET_WM_STATE_MAXIMIZED_VERT"), has("_NET_WM_STATE_MAXIMIZED_HORZ"));
	let before = geometry(&conn, root, win)?;
	// A maximized window can't take a size: unmaximize it first (re-maximized on restore).
	set_maximized(&conn, root, win, false, maximized);
	// The WM's frame around the client: left, right, top, bottom.
	let ext = prop32(&conn, win, atom(&conn, "_NET_FRAME_EXTENTS"), AtomEnum::CARDINAL);
	let (l, r, t, b) = match ext.as_slice() {
		[l, r, t, b, ..] => (*l as f64, *r as f64, *t as f64, *b as f64),
		_ => (0.0, 0.0, 0.0, 0.0),
	};
	let work = super::area_of(&before, &work_areas(&conn, root, desk))?;
	let (cw, ch) = super::fit_client(
		pane_w,
		pane_h,
		(work.w - l - r).max(1.0) as u32,
		(work.h - t - b).max(1.0) as u32,
	);
	let (cw, ch) = (cw as f64, ch as f64);
	// Keep the whole frame on the window's monitor.
	let x = super::place(before.x - l, cw + l + r, work.x, work.w) + l;
	let y = super::place(before.y - t, ch + t + b, work.y, work.h) + t;
	move_resize(&conn, root, win, WindowRect { x, y, w: cw, h: ch });
	wait_for_size(&conn, root, win, before);
	tracing::info!(id, pane = %format!("{pane_w}x{pane_h}"), client = %format!("{cw}x{ch}"), "window-fit: resized the X11 window to the split pane's shape");
	Some(FitPrev { id, rect: before, maximized })
}

pub(super) fn restore(prev: &FitPrev) {
	let Some((conn, root, _)) = open() else { return };
	let Ok(win) = u32::try_from(prev.id) else { return };
	let Some(now) = geometry(&conn, root, win) else {
		return; // closed meanwhile — nothing to restore
	};
	move_resize(&conn, root, win, prev.rect);
	set_maximized(&conn, root, win, true, prev.maximized);
	wait_for_size(&conn, root, win, now);
	tracing::info!(id = prev.id, "window-fit: restored the X11 window");
}
