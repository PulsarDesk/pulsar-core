//! Linux **per-window** mouse + keyboard injection for X11 (and XWayland windows)
//! — the Linux side of same-host co-op. Mirrors the Windows `window_win::WindowInput`
//! surface so the host's `on_input` closure needs no per-OS branching.
//!
//! ## How it works
//!
//! * **Target window in the background** (the normal co-op case: two panes, one
//!   focus): events are delivered with core-protocol `SendEvent` straight to the
//!   target window (the deepest child under the cached pointer for mouse events, the
//!   top-level for keys). Nothing global moves: the system pointer, the keyboard
//!   focus and the other pane's window are untouched.
//! * **Target window in front** (`_NET_ACTIVE_WINDOW`, or the input focus inside
//!   it): mouse goes through **XTest** at the window's root coordinates, i.e. as a
//!   real-looking device event that also reaches XInput2 clients. The host routes the
//!   keyboard of a front window through the global uinput injector itself (see
//!   `host.rs`), exactly as on Windows.
//!
//! ## HONEST CEILING (read before relying on this for games)
//!
//! `SendEvent` events carry the `send_event` flag. Many clients ignore them:
//!   * **XInput2 clients** (GTK3/4 pointer input, Qt5/6 pointer input, SDL2/3 relative
//!     mouse, most modern games) select XI2 events and never look at core events, so
//!     a background-window click/move does not reach them; keys often still do
//!     (Qt/GTK read keyboard from core/XKB events) but not always;
//!   * **xterm** and other Xt apps drop synthetic events unless `allowSendEvents`;
//!   * games reading `/dev/input` or raw XI2 never see either.
//!   That is an X11 protocol limit, not a missing feature: the only per-window channel
//!   the X server offers to another client is `SendEvent`. `uinput` cannot target a
//!   window at all (it is a kernel input device, seen by whoever holds focus), and
//!   XTest drives the one global pointer/focus.
//! * **Native Wayland windows:** impossible. Wayland has no protocol for one client to
//!   inject input into another client's surface (by design); the RemoteDesktop portal
//!   and uinput are both seat-global. Only XWayland windows (which have an X window
//!   id) can be targeted, through this same X11 path. `new` returns `None` without an
//!   X display, and the caller keeps the global injector.

use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
	self, AtomEnum, ButtonPressEvent, ConnectionExt as _, EventMask, KeyPressEvent,
	MotionNotifyEvent, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::window_common::{
	char_to_keysym, clamp_to_area, find_keysym, free_keycodes, norm_to_area, x11_button,
	x11_button_mask, x11_keycode, x11_modifier_mask, ScrollAcc,
};

/// How many otherwise-unused keycodes `type_char` may borrow (round-robin), so a
/// remap for the next character doesn't race the client still translating the last.
const SCRATCH_KEYCODES: usize = 4;
/// Caps Lock toggles `LockMask` in the event state.
const LOCK_MASK: u16 = 1 << 1;
/// Window size / front / alive state and the child under the pointer are re-read at
/// most this often: each is one or more synchronous X round-trips, and the host asks
/// on every input event (mouse moves arrive at 120 Hz and more).
const CACHE_TTL: Duration = Duration::from_millis(200);

/// The deepest mapped child of the target under the pointer, cached: its origin in
/// target-window coordinates and its size, so a point that stays inside it needs no
/// round-trip. Plus the target's own origin in root coordinates.
#[derive(Clone, Copy)]
struct Located {
	at: Instant,
	child: Window,
	/// Child origin in target-window coordinates (0,0 when the child IS the target).
	off_x: i16,
	off_y: i16,
	w: i16,
	h: i16,
	/// Target-window origin in root coordinates.
	root_x: i16,
	root_y: i16,
}

impl Located {
	fn contains(&self, x: i16, y: i16) -> bool {
		x >= self.off_x
			&& y >= self.off_y
			&& x < self.off_x.saturating_add(self.w)
			&& y < self.off_y.saturating_add(self.h)
	}
}

pub struct WindowInput {
	conn: RustConnection,
	root: Window,
	win: Window,
	net_active: u32,
	last_x: f64,
	last_y: f64,
	/// Held buttons (our ids) — `state` mask for move/scroll events + teardown release.
	held_buttons: HashSet<u8>,
	/// Held X keycodes, released on flush/Drop.
	held_keys: HashSet<u8>,
	/// Held modifier mask (Shift/Control/Mod1/Mod4/Mod5) + Caps Lock latch.
	mods: u16,
	caps: bool,
	scroll: ScrollAcc,
	/// Cached keyboard mapping (refreshed after every scratch remap).
	keymap: Vec<u32>,
	syms_per: usize,
	first_keycode: u8,
	/// Borrowed scratch keycodes and the next one to use; restored on Drop.
	scratch: Vec<u8>,
	scratch_next: usize,
	scratch_used: HashSet<u8>,
	/// Whether the X server has XTest (front-window mouse path).
	xtest: bool,
	/// Cached `in_front` / `size` answers and when they were read (see `CACHE_TTL`).
	front: Cell<Option<(Instant, bool)>>,
	alive: Cell<Option<(Instant, bool)>>,
	geom: Cell<Option<(Instant, (f64, f64))>>,
	located: Cell<Option<Located>>,
}

impl WindowInput {
	/// Target X window `id` (an XID as an `i64`, as the capture path publishes it).
	/// `None` if there is no X display (pure Wayland), or the window is gone.
	pub fn new(id: i64) -> Option<Self> {
		let win = u32::try_from(id).ok().filter(|&w| w != 0)?;
		let (conn, screen) = x11rb::connect(None).ok()?;
		let root = conn.setup().roots.get(screen)?.root;
		conn.get_geometry(win).ok()?.reply().ok()?;
		let net_active = conn
			.intern_atom(false, b"_NET_ACTIVE_WINDOW")
			.ok()
			.and_then(|c| c.reply().ok())
			.map(|r| r.atom)
			.unwrap_or(0);
		let xtest = conn
			.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
			.ok()
			.flatten()
			.is_some();
		let mut me = Self {
			conn,
			root,
			win,
			net_active,
			last_x: 0.0,
			last_y: 0.0,
			held_buttons: HashSet::new(),
			held_keys: HashSet::new(),
			mods: 0,
			caps: false,
			scroll: ScrollAcc::default(),
			keymap: Vec::new(),
			syms_per: 0,
			first_keycode: 8,
			scratch: Vec::new(),
			scratch_next: 0,
			scratch_used: HashSet::new(),
			xtest,
			front: Cell::new(None),
			alive: Cell::new(None),
			geom: Cell::new(None),
			located: Cell::new(None),
		};
		me.refresh_keymap();
		me.scratch = free_keycodes(&me.keymap, me.syms_per, me.first_keycode, SCRATCH_KEYCODES);
		Some(me)
	}

	fn refresh_keymap(&mut self) {
		let setup = self.conn.setup();
		let (min, max) = (setup.min_keycode, setup.max_keycode);
		if let Some(r) = self
			.conn
			.get_keyboard_mapping(min, max - min + 1)
			.ok()
			.and_then(|c| c.reply().ok())
		{
			self.syms_per = r.keysyms_per_keycode as usize;
			self.keymap = r.keysyms;
			self.first_keycode = min;
		}
	}

	/// Is the target window still alive? (Cached for `CACHE_TTL`.)
	pub fn is_alive(&self) -> bool {
		if let Some((t, v)) = self.alive.get() {
			if t.elapsed() < CACHE_TTL {
				return v;
			}
		}
		let v = self
			.conn
			.get_geometry(self.win)
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some();
		self.alive.set(Some((Instant::now(), v)));
		v
	}

	fn parent_of(&self, w: Window) -> Option<Window> {
		let r = self.conn.query_tree(w).ok()?.reply().ok()?;
		(r.parent != 0 && w != r.root).then_some(r.parent)
	}

	/// `a` is `b` or one of its ancestors (window-manager frames sit above clients).
	fn is_ancestor_or_self(&self, a: Window, mut b: Window) -> bool {
		for _ in 0..32 {
			if a == b {
				return true;
			}
			match self.parent_of(b) {
				Some(p) => b = p,
				None => return false,
			}
		}
		false
	}

	/// Is the target the active (front) window? EWMH `_NET_ACTIVE_WINDOW` first, then
	/// the core input focus (WMs without EWMH). Either may name the client window or
	/// its frame, so ancestry is checked both ways.
	pub fn in_front(&self) -> bool {
		if let Some((t, v)) = self.front.get() {
			if t.elapsed() < CACHE_TTL {
				return v;
			}
		}
		let v = self.query_front();
		self.front.set(Some((Instant::now(), v)));
		v
	}

	/// `a` and the target are the same window, or one contains the other.
	fn related(&self, a: Window) -> bool {
		self.is_ancestor_or_self(a, self.win) || self.is_ancestor_or_self(self.win, a)
	}

	/// Front = the X input focus is in the target's tree, and (with EWMH) the WM's
	/// `_NET_ACTIVE_WINDOW` names it too. The focus check matters on XWayland: Mutter /
	/// KWin can leave `_NET_ACTIVE_WINDOW` stale when focus moves to a native Wayland
	/// window, while the X focus moves off the target. A stale "front" would send the
	/// background window's mouse through XTest and move the shared X pointer.
	fn query_front(&self) -> bool {
		let Some(focus) = self
			.conn
			.get_input_focus()
			.ok()
			.and_then(|c| c.reply().ok())
			.map(|r| r.focus)
		else {
			return false;
		};
		let active = (self.net_active != 0)
			.then(|| {
				self.conn
					.get_property(false, self.root, self.net_active, AtomEnum::WINDOW, 0, 1)
					.ok()?
					.reply()
					.ok()?
					.value32()?
					.next()
			})
			.flatten()
			.filter(|&w| w != 0);
		front_verdict(focus, active.map(|a| self.related(a)), || self.related(focus))
	}

	fn size(&self) -> (f64, f64) {
		if let Some((t, v)) = self.geom.get() {
			if t.elapsed() < CACHE_TTL {
				return v;
			}
		}
		let v = self.query_size();
		self.geom.set(Some((Instant::now(), v)));
		v
	}

	fn query_size(&self) -> (f64, f64) {
		self.conn
			.get_geometry(self.win)
			.ok()
			.and_then(|c| c.reply().ok())
			.map(|g| (g.width as f64, g.height as f64))
			.unwrap_or((0.0, 0.0))
	}

	/// The deepest mapped child of the target under window point (x, y), with the
	/// point in that child's coordinates, plus the point in root coordinates. The
	/// child + offsets are cached for `CACHE_TTL` while the point stays inside it.
	fn locate(&self, x: i16, y: i16) -> (Window, i16, i16, i16, i16) {
		let fresh = self.located.get().filter(|l| l.at.elapsed() < CACHE_TTL && l.contains(x, y));
		let l = match fresh {
			Some(l) => l,
			None => {
				let l = self.query_locate(x, y);
				self.located.set(Some(l));
				l
			}
		};
		(
			l.child,
			x.saturating_sub(l.off_x),
			y.saturating_sub(l.off_y),
			l.root_x.saturating_add(x),
			l.root_y.saturating_add(y),
		)
	}

	/// The target's origin in root coordinates, from the cached `locate` answer when
	/// it is fresh (keys need it only for the event's root_x/root_y fields).
	fn root_origin(&self) -> (i16, i16) {
		match self.located.get().filter(|l| l.at.elapsed() < CACHE_TTL) {
			Some(l) => (l.root_x, l.root_y),
			None => {
				let (x, y) = self.point();
				let l = self.query_locate(x, y);
				self.located.set(Some(l));
				(l.root_x, l.root_y)
			}
		}
	}

	fn query_locate(&self, x: i16, y: i16) -> Located {
		let (w, h) = self.size();
		let (mut cur, mut cx, mut cy) = (self.win, x, y);
		for _ in 0..16 {
			let Some(r) = self
				.conn
				.translate_coordinates(cur, cur, cx, cy)
				.ok()
				.and_then(|c| c.reply().ok())
			else {
				break;
			};
			if r.child == 0 {
				break;
			}
			let Some(t) = self
				.conn
				.translate_coordinates(cur, r.child, cx, cy)
				.ok()
				.and_then(|c| c.reply().ok())
			else {
				break;
			};
			cur = r.child;
			cx = t.dst_x;
			cy = t.dst_y;
		}
		let (cw, ch) = if cur == self.win {
			(w as i16, h as i16)
		} else {
			self.conn
				.get_geometry(cur)
				.ok()
				.and_then(|c| c.reply().ok())
				.map_or((1, 1), |g| (g.width as i16, g.height as i16))
		};
		let (root_x, root_y) = self
			.conn
			.translate_coordinates(self.win, self.root, 0, 0)
			.ok()
			.and_then(|c| c.reply().ok())
			.map_or((0, 0), |r| (r.dst_x, r.dst_y));
		Located {
			at: Instant::now(),
			child: cur,
			off_x: x.saturating_sub(cx),
			off_y: y.saturating_sub(cy),
			w: cw.max(1),
			h: ch.max(1),
			root_x,
			root_y,
		}
	}

	fn state(&self) -> u16 {
		let mut s = self.mods;
		if self.caps {
			s |= LOCK_MASK;
		}
		for &b in &self.held_buttons {
			s |= x11_button_mask(b);
		}
		s
	}

	fn point(&self) -> (i16, i16) {
		(self.last_x.round() as i16, self.last_y.round() as i16)
	}

	/// Send a pointer event: XTest when the window is in front (real device-level
	/// input, reaches XI2 clients), `SendEvent` to the child under the point otherwise.
	/// `kind` = MotionNotify / ButtonPress / ButtonRelease, `detail` = button number.
	fn pointer_event(&self, kind: u8, detail: u8) {
		let (x, y) = self.point();
		let (child, cx, cy, rx, ry) = self.locate(x, y);
		if self.xtest && self.in_front() {
			let _ = self.conn.xtest_fake_input(
				xproto::MOTION_NOTIFY_EVENT,
				0,
				0,
				self.root,
				rx,
				ry,
				0,
			);
			if kind != xproto::MOTION_NOTIFY_EVENT {
				let _ = self.conn.xtest_fake_input(kind, detail, 0, self.root, 0, 0, 0);
			}
			let _ = self.conn.flush();
			return;
		}
		let state = self.state();
		if kind == xproto::MOTION_NOTIFY_EVENT {
			let ev = MotionNotifyEvent {
				response_type: kind,
				detail: xproto::Motion::NORMAL,
				sequence: 0,
				time: x11rb::CURRENT_TIME,
				root: self.root,
				event: child,
				child: x11rb::NONE,
				root_x: rx,
				root_y: ry,
				event_x: cx,
				event_y: cy,
				state: state.into(),
				same_screen: true,
			};
			let mask = EventMask::POINTER_MOTION | EventMask::BUTTON_MOTION;
			let _ = self.conn.send_event(true, child, mask, ev);
		} else {
			let ev = ButtonPressEvent {
				response_type: kind,
				detail,
				sequence: 0,
				time: x11rb::CURRENT_TIME,
				root: self.root,
				event: child,
				child: x11rb::NONE,
				root_x: rx,
				root_y: ry,
				event_x: cx,
				event_y: cy,
				state: state.into(),
				same_screen: true,
			};
			let mask = if kind == xproto::BUTTON_PRESS_EVENT {
				EventMask::BUTTON_PRESS
			} else {
				EventMask::BUTTON_RELEASE
			};
			let _ = self.conn.send_event(true, child, mask, ev);
		}
		let _ = self.conn.flush();
	}

	/// Send a key event with `SendEvent` to the target top-level (toolkits dispatch
	/// to their focus widget from there).
	fn key_event(&self, keycode: u8, down: bool, state: u16) {
		let (x, y) = self.point();
		let (ox, oy) = self.root_origin();
		let (rx, ry) = (ox.saturating_add(x), oy.saturating_add(y));
		let kind = if down {
			xproto::KEY_PRESS_EVENT
		} else {
			xproto::KEY_RELEASE_EVENT
		};
		let ev = KeyPressEvent {
			response_type: kind,
			detail: keycode,
			sequence: 0,
			time: x11rb::CURRENT_TIME,
			root: self.root,
			event: self.win,
			child: x11rb::NONE,
			root_x: rx,
			root_y: ry,
			event_x: x,
			event_y: y,
			state: state.into(),
			same_screen: true,
		};
		let mask = if down {
			EventMask::KEY_PRESS
		} else {
			EventMask::KEY_RELEASE
		};
		let _ = self.conn.send_event(true, self.win, mask, ev);
		let _ = self.conn.flush();
	}

	/// Move to a normalized (0..1) position on the target window.
	pub fn pointer(&mut self, x: f64, y: f64) {
		let (w, h) = self.size();
		(self.last_x, self.last_y) = norm_to_area(x, y, w, h);
		self.pointer_event(xproto::MOTION_NOTIFY_EVENT, 0);
	}

	/// Advance the cached pointer by a raw pixel delta.
	pub fn pointer_relative(&mut self, dx: f64, dy: f64) {
		let (w, h) = self.size();
		(self.last_x, self.last_y) = clamp_to_area(self.last_x + dx, self.last_y + dy, w, h);
		self.pointer_event(xproto::MOTION_NOTIFY_EVENT, 0);
	}

	/// Press/release a mouse button (0=left, 1=right, 2=middle) at the cached point.
	pub fn button(&mut self, button: u8, down: bool) {
		// X reports the state BEFORE the event: send first, then update the held set.
		let kind = if down {
			xproto::BUTTON_PRESS_EVENT
		} else {
			xproto::BUTTON_RELEASE_EVENT
		};
		self.pointer_event(kind, x11_button(button));
		if down {
			self.held_buttons.insert(button);
		} else {
			self.held_buttons.remove(&button);
		}
	}

	/// Scroll: X11 wheels are buttons 4/5 (vertical) and 6/7 (horizontal), one
	/// press+release per notch.
	pub fn scroll(&mut self, dx: f64, dy: f64) {
		let (v, h) = self.scroll.push(dx, dy);
		let (vb, hb) = (if v > 0 { 5 } else { 4 }, if h > 0 { 7 } else { 6 });
		for (n, b) in [(v.unsigned_abs(), vb), (h.unsigned_abs(), hb)] {
			for _ in 0..n.min(20) {
				self.pointer_event(xproto::BUTTON_PRESS_EVENT, b);
				self.pointer_event(xproto::BUTTON_RELEASE_EVENT, b);
			}
		}
	}

	/// Press/release a key by evdev keycode (X keycode = evdev + 8).
	pub fn key(&mut self, code: u32, down: bool) {
		let Some(kc) = x11_keycode(code) else {
			return;
		};
		if code == 58 && down && !self.held_keys.contains(&kc) {
			self.caps = !self.caps;
		}
		let state = self.state();
		self.key_event(kc, down, state);
		if let Some(m) = x11_modifier_mask(code) {
			if down {
				self.mods |= m;
			} else {
				self.mods &= !m;
			}
		}
		if down {
			self.held_keys.insert(kc);
		} else {
			self.held_keys.remove(&kc);
		}
	}

	/// Type a resolved Unicode character. Uses the keycode the host layout already
	/// has for it (with Shift for level 1); a character the layout lacks borrows an
	/// unused keycode, maps the keysym onto it (as `xdotool type` does) and types it.
	pub fn type_char(&mut self, c: char) {
		let Some(sym) = char_to_keysym(c) else {
			return;
		};
		// The cached keymap includes the scratch keycodes as last remapped, so a char
		// typed again finds its scratch keycode here and does NOT remap: the
		// server-global ChangeKeyboardMapping (MappingNotify to every client) runs only
		// for a char that is on no keycode yet.
		let found = find_keysym(&self.keymap, self.syms_per, self.first_keycode, sym);
		let (kc, shift) = match found {
			Some(f) => f,
			None => {
				let Some(&kc) = self.scratch.get(self.scratch_next) else {
					tracing::debug!(?c, "window input: no free keycode to type this char");
					return;
				};
				self.scratch_next = (self.scratch_next + 1) % self.scratch.len();
				let per = self.syms_per.max(1);
				let mut syms = vec![0u32; per];
				syms[0] = sym;
				if per > 1 {
					syms[1] = sym;
				}
				let _ = self
					.conn
					.change_keyboard_mapping(1, kc, per as u8, &syms)
					.map(|c| c.check());
				self.scratch_used.insert(kc);
				self.refresh_keymap();
				(kc, false)
			}
		};
		// The char is already resolved: drop the held modifiers so e.g. a held Ctrl
		// doesn't turn it into a shortcut; add Shift only when the level needs it.
		let state = if shift { 1 } else { 0 };
		self.key_event(kc, true, state);
		self.key_event(kc, false, state);
	}

	/// Release every held key + button on the target window. Idempotent.
	pub fn flush_held(&mut self) {
		for b in self.held_buttons.clone() {
			self.button(b, false);
		}
		self.held_buttons.clear();
		for kc in std::mem::take(&mut self.held_keys) {
			let state = self.state();
			self.key_event(kc, false, state);
		}
		self.mods = 0;
	}
}

impl Drop for WindowInput {
	fn drop(&mut self) {
		if self.is_alive() {
			self.flush_held();
		}
		// Give the borrowed scratch keycodes back (NoSymbol) so the layout is as found.
		let per = self.syms_per.max(1);
		for kc in std::mem::take(&mut self.scratch_used) {
			let _ = self.conn.change_keyboard_mapping(1, kc, per as u8, &vec![0u32; per]);
		}
		let _ = self.conn.flush();
	}
}

/// The front decision from its inputs: the X `focus` window (0 = None, 1 =
/// PointerRoot), whether `_NET_ACTIVE_WINDOW` (when set) is related to the target, and
/// a lazy "is `focus` related to the target" check.
fn front_verdict(focus: Window, active_related: Option<bool>, focus_related: impl FnOnce() -> bool) -> bool {
	match (focus, active_related) {
		// Focus None: no X window has the keyboard (XWayland: a Wayland window does).
		(0, _) => false,
		// PointerRoot focus follows the pointer: trust the WM's active window.
		(1, a) => a == Some(true),
		(_, Some(false)) => false,
		(_, _) => focus_related(),
	}
}

// The host keeps this in its `on_input` closure state (a tokio task) → must be Send.
const _: fn() = || {
	fn assert_send<T: Send>() {}
	assert_send::<WindowInput>();
};

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn front_needs_focus_inside_target() {
		// XWayland after focus moved to a native Wayland window: stale active, focus None.
		assert!(!front_verdict(0, Some(true), || true));
		// Active names the target but the X focus is elsewhere.
		assert!(!front_verdict(42, Some(true), || false));
		// Active names another window.
		assert!(!front_verdict(42, Some(false), || true));
		// Both agree.
		assert!(front_verdict(42, Some(true), || true));
		// No EWMH: focus alone decides.
		assert!(front_verdict(42, None, || true));
		assert!(!front_verdict(42, None, || false));
		// PointerRoot: the WM's active window decides.
		assert!(front_verdict(1, Some(true), || false));
		assert!(!front_verdict(1, None, || true));
	}

	#[test]
	fn located_contains_is_half_open() {
		let l = Located { at: Instant::now(), child: 1, off_x: 10, off_y: 20, w: 5, h: 5, root_x: 0, root_y: 0 };
		assert!(l.contains(10, 20));
		assert!(l.contains(14, 24));
		assert!(!l.contains(15, 20));
		assert!(!l.contains(9, 20));
	}
}
