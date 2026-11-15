//! Linux host: which key (and level) types a character on the HOST's own keyboard layout.
//!
//! A client resolves a printable key through ITS layout and sends the character
//! (`InputEvent::Char`). uinput only has keycodes, and the compositor / X server turns them
//! into characters with the host session's layout, so the host must find the key that layout
//! uses for the character: a TR-Q host types `ş` with KEY_SEMICOLON and `@` with AltGr+Q,
//! where a fixed US table typed `;` and Shift+2 (`'` on TR-Q).
//!
//! The layout comes from, in order:
//! 1. **Wayland:** the keymap the compositor hands every client on `wl_keyboard` — the exact
//!    keymap it applies to our uinput keyboard too (GNOME, KDE, wlroots alike).
//! 2. **X11:** the root window's `_XKB_RULES_NAMES` (what `setxkbmap` / the desktop set).
//! 3. `XKB_DEFAULT_*` environment, `/etc/X11/xorg.conf.d/00-keyboard.conf`,
//!    `/etc/default/keyboard`, else libxkbcommon's default (`us`).
//!
//! Known limits: only the first layout group is used (a `tr,us` session switched to its
//! second group types with the first group's keys), Caps Lock is tracked only for the
//! presses Pulsar injects itself, and a character the layout lacks goes through
//! [`X11Scratch`] (X11) or the Unicode entry (GTK / IBus, see `uinput.rs`).

use std::collections::HashMap;
use std::time::Instant;

use xkbcommon::xkb;

use super::keytables::{
	build_char_table, parse_default_keyboard, parse_xkb_rules_names, parse_xorg_keyboard_conf, us_layout,
	KeyChord, XkbNames, EVDEV_ALTGR, EVDEV_LSHIFT,
};

/// The host layout's character → key table, and where it came from (for the log).
pub(super) struct HostKeymap {
	pub table: HashMap<char, KeyChord>,
	pub source: &'static str,
	pub built: Instant,
}

impl HostKeymap {
	/// Read the host layout now (a Wayland round-trip or an X property read, plus an xkb
	/// compile: a few ms).
	pub fn load() -> Self {
		let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
		let (keymap, source) = load_keymap(&ctx);
		let (table, source) = match keymap.as_ref().map(char_table) {
			Some(t) if !t.is_empty() => (t, source),
			// No keymap compiled (no xkeyboard-config data): the built-in US table.
			_ => (build_char_table(us_layout), "builtin-us"),
		};
		tracing::info!(source, chars = table.len(), "host keyboard layout for typed characters");
		Self { table, source, built: Instant::now() }
	}
}

fn wayland_session() -> bool {
	std::env::var_os("WAYLAND_DISPLAY").is_some()
		|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"))
}

fn load_keymap(ctx: &xkb::Context) -> (Option<xkb::Keymap>, &'static str) {
	if wayland_session() {
		if let Some(k) = wayland::keymap(ctx) {
			return (Some(k), "wayland");
		}
	} else if let Some(names) = x11_rules_names() {
		if let Some(k) = compile(ctx, &names) {
			return (Some(k), "x11");
		}
	}
	let env = |k: &str| std::env::var(k).unwrap_or_default();
	let from_env = XkbNames {
		rules: env("XKB_DEFAULT_RULES"),
		model: env("XKB_DEFAULT_MODEL"),
		layout: env("XKB_DEFAULT_LAYOUT"),
		variant: env("XKB_DEFAULT_VARIANT"),
		options: env("XKB_DEFAULT_OPTIONS"),
	};
	let read = |p: &str| std::fs::read_to_string(p).ok();
	let candidates = [
		(Some(from_env).filter(|n| !n.is_empty()), "env"),
		(read("/etc/X11/xorg.conf.d/00-keyboard.conf").as_deref().and_then(parse_xorg_keyboard_conf), "xorg.conf"),
		(read("/etc/default/keyboard").as_deref().and_then(parse_default_keyboard), "default/keyboard"),
	];
	for (names, source) in candidates {
		if let Some(k) = names.and_then(|n| compile(ctx, &n)) {
			return (Some(k), source);
		}
	}
	(compile(ctx, &XkbNames::default()), "default")
}

fn compile(ctx: &xkb::Context, n: &XkbNames) -> Option<xkb::Keymap> {
	let options = (!n.options.is_empty()).then(|| n.options.clone());
	xkb::Keymap::new_from_names(ctx, &n.rules, &n.model, &n.layout, &n.variant, options, xkb::KEYMAP_COMPILE_NO_FLAGS)
}

/// Build the table by asking xkb what each key types with Shift and/or AltGr really pressed
/// (the same physical keys the injector then presses), so the answer is exactly what the
/// compositor computes for the injected events.
fn char_table(keymap: &xkb::Keymap) -> HashMap<char, KeyChord> {
	let held = |shift: bool, altgr: bool| {
		let mut st = xkb::State::new(keymap);
		if shift {
			st.update_key(xkb::Keycode::new(EVDEV_LSHIFT as u32 + 8), xkb::KeyDirection::Down);
		}
		if altgr {
			st.update_key(xkb::Keycode::new(EVDEV_ALTGR as u32 + 8), xkb::KeyDirection::Down);
		}
		st
	};
	let states = [held(false, false), held(true, false), held(false, true), held(true, true)];
	build_char_table(|code, shift, altgr| {
		let st = &states[shift as usize + 2 * altgr as usize];
		char::from_u32(st.key_get_utf32(xkb::Keycode::new(code as u32 + 8))).filter(|&c| c != '\0')
	})
}

/// The X root window's `_XKB_RULES_NAMES`, `None` without an X display.
fn x11_rules_names() -> Option<XkbNames> {
	use x11rb::connection::Connection as _;
	use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
	let (conn, screen) = x11rb::connect(None).ok()?;
	let root = conn.setup().roots.get(screen)?.root;
	let atom = conn.intern_atom(true, b"_XKB_RULES_NAMES").ok()?.reply().ok()?.atom;
	if atom == 0 {
		return None;
	}
	let prop = conn.get_property(false, root, atom, AtomEnum::STRING, 0, 1024).ok()?.reply().ok()?;
	parse_xkb_rules_names(&prop.value)
}

/// The compositor's keymap, read the way every Wayland client gets it: bind `wl_seat`, ask
/// for its keyboard, take the `keymap` event's fd.
mod wayland {
	use std::os::fd::OwnedFd;
	use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
	use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle, WEnum};
	use xkbcommon::xkb;

	#[derive(Default)]
	struct State {
		seat: Option<wl_seat::WlSeat>,
		has_keyboard: bool,
		keymap: Option<(OwnedFd, u32)>,
	}

	impl Dispatch<wl_registry::WlRegistry, ()> for State {
		fn event(
			state: &mut Self,
			registry: &wl_registry::WlRegistry,
			event: wl_registry::Event,
			_: &(),
			_: &Connection,
			qh: &QueueHandle<Self>,
		) {
			if let wl_registry::Event::Global { name, interface, version } = event {
				if interface == "wl_seat" && state.seat.is_none() {
					state.seat = Some(registry.bind(name, version.min(5), qh, ()));
				}
			}
		}
	}

	impl Dispatch<wl_seat::WlSeat, ()> for State {
		fn event(state: &mut Self, _: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
			if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = event {
				state.has_keyboard = c.contains(wl_seat::Capability::Keyboard);
			}
		}
	}

	impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
		fn event(
			state: &mut Self,
			_: &wl_keyboard::WlKeyboard,
			event: wl_keyboard::Event,
			_: &(),
			_: &Connection,
			_: &QueueHandle<Self>,
		) {
			if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } = event {
				state.keymap = Some((fd, size));
			}
		}
	}

	pub(super) fn keymap(ctx: &xkb::Context) -> Option<xkb::Keymap> {
		let conn = Connection::connect_to_env().ok()?;
		let mut queue = conn.new_event_queue();
		let qh = queue.handle();
		conn.display().get_registry(&qh, ());
		let mut state = State::default();
		queue.roundtrip(&mut state).ok()?; // globals → seat bound
		queue.roundtrip(&mut state).ok()?; // seat capabilities
		if !state.has_keyboard {
			return None;
		}
		let keyboard = state.seat.as_ref()?.get_keyboard(&qh, ());
		queue.roundtrip(&mut state).ok()?; // keymap
		let km = state.keymap.take().and_then(|(fd, size)| {
			// SAFETY: the compositor's keymap fd, `size` bytes, mapped read-only/private as the
			// protocol requires (libxkbcommon's `new_from_fd`).
			unsafe { xkb::Keymap::new_from_fd(ctx, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS) }
				.ok()
				.flatten()
		});
		if keyboard.version() >= 3 {
			keyboard.release();
		}
		let _ = conn.flush();
		km
	}
}

/// X11 only: types a character the layout lacks by borrowing an unused keycode, mapping the
/// character's keysym onto it (`ChangeKeyboardMapping`, applied by the server to every
/// keyboard including our uinput one) and pressing that key — what `xdotool type` does.
/// The borrowed keycodes are given back on drop.
pub(super) struct X11Scratch {
	conn: x11rb::rust_connection::RustConnection,
	syms_per: usize,
	scratch: Vec<u8>,
	next: usize,
	/// Characters currently mapped on a scratch keycode.
	mapped: HashMap<char, u8>,
}

/// How many keycodes the scratch remap may borrow (round-robin), so a remap for the next
/// character doesn't race a client still translating the previous one.
const SCRATCH_KEYCODES: usize = 4;

impl X11Scratch {
	/// `None` on Wayland (the compositor, not X, translates uinput keys there) or without a
	/// usable X display.
	pub fn new() -> Option<Self> {
		use x11rb::connection::Connection as _;
		use x11rb::protocol::xproto::ConnectionExt as _;
		if wayland_session() {
			return None;
		}
		let (conn, _) = x11rb::connect(None).ok()?;
		let setup = conn.setup();
		let (min, max) = (setup.min_keycode, setup.max_keycode);
		let map = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
		let syms_per = map.keysyms_per_keycode as usize;
		// Keycodes the uinput keyboard can press (evdev 1..=248 → X 9..=256).
		let scratch: Vec<u8> = super::window_common::free_keycodes(&map.keysyms, syms_per, min, 64)
			.into_iter()
			.filter(|&kc| kc >= 9)
			.take(SCRATCH_KEYCODES)
			.collect();
		(!scratch.is_empty()).then(|| Self { conn, syms_per, scratch, next: 0, mapped: HashMap::new() })
	}

	/// The evdev code that types `c` after mapping it onto a scratch keycode.
	pub fn evdev_for(&mut self, c: char) -> Option<u16> {
		use x11rb::connection::Connection as _;
		use x11rb::protocol::xproto::ConnectionExt as _;
		if let Some(&kc) = self.mapped.get(&c) {
			return Some(kc as u16 - 8);
		}
		let sym = super::window_common::char_to_keysym(c)?;
		let kc = *self.scratch.get(self.next)?;
		self.next = (self.next + 1) % self.scratch.len();
		let per = self.syms_per.max(1);
		let mut syms = vec![0u32; per];
		syms[0] = sym;
		if per > 1 {
			syms[1] = sym;
		}
		self.conn.change_keyboard_mapping(1, kc, per as u8, &syms).ok()?.check().ok()?;
		let _ = self.conn.flush();
		self.mapped.retain(|_, &mut k| k != kc);
		self.mapped.insert(c, kc);
		Some(kc as u16 - 8)
	}
}

impl Drop for X11Scratch {
	fn drop(&mut self) {
		use x11rb::connection::Connection as _;
		use x11rb::protocol::xproto::ConnectionExt as _;
		let per = self.syms_per.max(1);
		for &kc in self.mapped.values() {
			let _ = self.conn.change_keyboard_mapping(1, kc, per as u8, &vec![0u32; per]);
		}
		let _ = self.conn.flush();
	}
}
