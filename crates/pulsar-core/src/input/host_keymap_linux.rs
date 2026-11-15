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
//! The layout is read on a process-wide worker thread (Wayland: our own `wl_keyboard`, kept
//! open, rebuilt on every keymap event; otherwise re-read every 60 s and, rate limited, when a
//! character is missing), never on the session's input path: `type_char` only reads the last
//! published tables (the built-in US one until the first read is done).
//!
//! Known limits: only the first layout group is used (a `tr,us` session switched to its
//! second group types with the first group's keys), Caps Lock is tracked only for the
//! presses Pulsar injects itself (and only where KEY_CAPSLOCK locks Caps on the host layout),
//! a character the layout has only as a dead key is typed as dead key + Space (or the dead key
//! twice), and one the layout lacks goes through [`X11Scratch`] (X11) or the Unicode entry
//! (GTK / IBus, see `uinput.rs`).

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use xkbcommon::xkb;

use super::keytables::{
	build_char_table, build_dead_table, parse_default_keyboard, parse_xkb_rules_names, parse_xorg_keyboard_conf,
	us_layout, us_layout_caps, DeadKey, KeyChord, ScratchSlots, XkbNames, EVDEV_ALTGR, EVDEV_LSHIFT,
};

/// The host layout's character tables, and where they came from (for the log).
pub(super) struct HostKeymap {
	/// Character → key + level with Caps Lock off.
	pub table: HashMap<char, KeyChord>,
	/// The same with Caps Lock locked (letters swap levels as the layout's key types say:
	/// TR-Q AltGr+a 'æ' is FOUR_LEVEL_SEMIALPHABETIC and does not).
	pub table_caps: HashMap<char, KeyChord>,
	/// Characters this layout has only as a dead key: the chords to tap (dead key, then Space or
	/// the dead key again).
	pub dead: HashMap<char, Vec<KeyChord>>,
	/// KEY_CAPSLOCK locks Caps on this layout. False under `caps:escape`, `ctrl:nocaps`,
	/// `caps:ctrl_modifier`, `grp:caps_toggle` and the like, where a forwarded Caps press must
	/// not flip the case of later typed letters.
	pub caps_locks: bool,
	pub source: &'static str,
}

impl HostKeymap {
	/// The tables of `keymap`, or the built-in US ones when the keymap types nothing.
	fn from_xkb(keymap: &xkb::Keymap, source: &'static str) -> Self {
		let km = tables(keymap, source);
		if km.table.is_empty() {
			Self::builtin_us()
		} else {
			km
		}
	}

	/// The built-in US tables: used until the worker has read the host layout, and when no
	/// keymap compiles at all (no xkeyboard-config data on a bare server).
	fn builtin_us() -> Self {
		Self {
			table: build_char_table(us_layout),
			table_caps: build_char_table(us_layout_caps),
			dead: HashMap::new(),
			caps_locks: true,
			source: "builtin-us",
		}
	}
}

/// The polled layout (X11 / no session) is re-read when it is this old.
const KEYMAP_MAX_AGE: Duration = Duration::from_secs(60);
/// A `Char` the layout lacks asks the worker to re-read it (the user may have switched
/// layouts), at most this often.
const KEYMAP_MISS_RELOAD: Duration = Duration::from_secs(5);

/// The process-wide keymap worker: owns every blocking layout read (a Wayland connection
/// with its round-trips, an X property read, an xkb compile: 5-50 ms, more on an RK3588 / Pi)
/// so the session's input path never waits on one.
struct Worker {
	current: Arc<Mutex<Option<Arc<HostKeymap>>>>,
	nudge: mpsc::SyncSender<()>,
}

fn worker() -> &'static Worker {
	static WORKER: OnceLock<Worker> = OnceLock::new();
	WORKER.get_or_init(|| {
		let current = Arc::new(Mutex::new(None));
		// One pending nudge is enough: more while a read runs say nothing new.
		let (nudge, nudges) = mpsc::sync_channel(1);
		let out = current.clone();
		if let Err(e) = std::thread::Builder::new().name("pulsar-host-keymap".into()).spawn(move || run(out, nudges)) {
			tracing::warn!(err = %e, "host keymap worker failed to start; typing with the built-in US layout");
		}
		Worker { current, nudge }
	})
}

/// Start reading the host layout in the background (called when the injector is created, so
/// the table is ready by the first typed character).
pub(super) fn start() {
	worker();
}

/// The current host layout tables. Never blocks on a layout read: the built-in US tables until
/// the worker has published the host's.
pub(super) fn current() -> Arc<HostKeymap> {
	static US: OnceLock<Arc<HostKeymap>> = OnceLock::new();
	let published = worker().current.lock().unwrap_or_else(|e| e.into_inner()).clone();
	published.unwrap_or_else(|| US.get_or_init(|| Arc::new(HostKeymap::builtin_us())).clone())
}

/// A character was missing from the table: ask the worker to re-read the layout (rate
/// limited there; a no-op while the Wayland keymap is watched live).
pub(super) fn nudge() {
	let _ = worker().nudge.try_send(());
}

fn run(current: Arc<Mutex<Option<Arc<HostKeymap>>>>, nudges: mpsc::Receiver<()>) {
	let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
	let publish = |km: HostKeymap| {
		tracing::info!(
			source = km.source,
			chars = km.table.len(),
			caps_locks = km.caps_locks,
			"host keyboard layout for typed characters"
		);
		*current.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(km));
	};
	// Wayland: keep our wl_keyboard and rebuild on every keymap the compositor sends (it sends
	// a new one when the session's layouts change) — no polling.
	if wayland_session() {
		if let Some(mut watch) = wayland::Watch::connect() {
			while let Some(keymap) = watch.next_keymap(&ctx) {
				if let Some(k) = keymap {
					publish(HostKeymap::from_xkb(&k, "wayland"));
				}
			}
			tracing::info!("host keymap: Wayland connection closed, polling the layout instead");
		}
	}
	loop {
		let (keymap, source) = load_keymap(&ctx);
		publish(match keymap {
			Some(k) => HostKeymap::from_xkb(&k, source),
			None => HostKeymap::builtin_us(),
		});
		let read_at = Instant::now();
		loop {
			let left = (read_at + KEYMAP_MAX_AGE).saturating_duration_since(Instant::now());
			match nudges.recv_timeout(left) {
				Ok(()) if read_at.elapsed() >= KEYMAP_MISS_RELOAD => break,
				Ok(()) => {} // too soon after the last read
				Err(mpsc::RecvTimeoutError::Timeout) => break,
				Err(mpsc::RecvTimeoutError::Disconnected) => return,
			}
		}
	}
}

fn wayland_session() -> bool {
	std::env::var_os("WAYLAND_DISPLAY").is_some()
		|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"))
}

fn load_keymap(ctx: &xkb::Context) -> (Option<xkb::Keymap>, &'static str) {
	if wayland_session() {
		if let Some(k) = wayland::Watch::connect().and_then(|mut w| w.next_keymap(ctx)).flatten() {
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

/// Build the tables by asking xkb what each key types with Shift and/or AltGr really pressed
/// (the same physical keys the injector then presses) and, for the Caps table, after a real
/// Caps Lock press — so every answer is exactly what the compositor computes for the injected
/// events, including how this layout's key types treat Lock.
fn tables(keymap: &xkb::Keymap, source: &'static str) -> HostKeymap {
	let kc = |evdev: u16| xkb::Keycode::new(evdev as u32 + 8);
	let state = |caps: bool, shift: bool, altgr: bool| {
		let mut st = xkb::State::new(keymap);
		if caps {
			st.update_key(kc(58), xkb::KeyDirection::Down);
			st.update_key(kc(58), xkb::KeyDirection::Up);
		}
		if shift {
			st.update_key(kc(EVDEV_LSHIFT), xkb::KeyDirection::Down);
		}
		if altgr {
			st.update_key(kc(EVDEV_ALTGR), xkb::KeyDirection::Down);
		}
		st
	};
	let caps_locks = state(true, false, false).mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_LOCKED);
	let levels = |caps: bool| {
		[state(caps, false, false), state(caps, true, false), state(caps, false, true), state(caps, true, true)]
	};
	let char_table = |states: &[xkb::State; 4]| {
		build_char_table(|code, shift, altgr| {
			let st = &states[shift as usize + 2 * altgr as usize];
			char::from_u32(st.key_get_utf32(kc(code))).filter(|&c| c != '\0')
		})
	};
	let plain = levels(false);
	let table = char_table(&plain);
	let table_caps = if caps_locks { char_table(&levels(true)) } else { table.clone() };
	let dead = build_dead_table(|code, shift, altgr| {
		DeadKey::from_keysym(plain[shift as usize + 2 * altgr as usize].key_get_one_sym(kc(code)).raw())
	});
	HostKeymap { table, table_caps, dead, caps_locks, source }
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
	use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle, WEnum};
	use xkbcommon::xkb;

	#[derive(Default)]
	pub(super) struct State {
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

	/// Our own `wl_keyboard`, kept for the compositor's keymap events.
	pub(super) struct Watch {
		conn: Connection,
		queue: EventQueue<State>,
		state: State,
		keyboard: wl_keyboard::WlKeyboard,
	}

	impl Watch {
		pub(super) fn connect() -> Option<Self> {
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
			Some(Self { conn, queue, state, keyboard })
		}

		/// Block until the compositor sends a keymap (the first one comes right after
		/// `get_keyboard`, later ones when the session's layouts change). `None` = the
		/// connection is gone; `Some(None)` = a keymap that did not compile.
		pub(super) fn next_keymap(&mut self, ctx: &xkb::Context) -> Option<Option<xkb::Keymap>> {
			while self.state.keymap.is_none() {
				self.queue.blocking_dispatch(&mut self.state).ok()?;
			}
			let (fd, size) = self.state.keymap.take()?;
			// SAFETY: the compositor's keymap fd, `size` bytes, mapped read-only/private as the
			// protocol requires (libxkbcommon's `new_from_fd`).
			let km = unsafe {
				xkb::Keymap::new_from_fd(ctx, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
			};
			Some(km.ok().flatten())
		}
	}

	impl Drop for Watch {
		fn drop(&mut self) {
			if self.keyboard.version() >= 3 {
				self.keyboard.release();
			}
			let _ = self.conn.flush();
		}
	}
}

/// X11 only: types a character the layout lacks by borrowing an unused keycode, mapping the
/// character's keysym onto it (`ChangeKeyboardMapping`, applied by the server to every
/// keyboard including our uinput one) and pressing that key — what `xdotool type` does.
/// The borrowed keycodes are given back on drop.
pub(super) struct X11Scratch {
	conn: x11rb::rust_connection::RustConnection,
	syms_per: usize,
	/// Borrowed keycodes and the character on each, least recently used reused first.
	slots: ScratchSlots,
}

/// How many keycodes the scratch remap may borrow. Enough for a language's extra letters in
/// both cases (Turkish on a US host: 12), so each is mapped once per session and later presses
/// cost no remap (a MappingNotify to every X client and a round-trip each).
const SCRATCH_KEYCODES: usize = 32;

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
		let slots = ScratchSlots::new(
			super::window_common::free_keycodes(&map.keysyms, syms_per, min, 64)
				.into_iter()
				.filter(|&kc| kc >= 9)
				.take(SCRATCH_KEYCODES),
		);
		(!slots.is_empty()).then(|| Self { conn, syms_per, slots })
	}

	/// The evdev code that types `c` after mapping it onto a scratch keycode (no X request
	/// when `c` is still mapped from an earlier press).
	pub fn evdev_for(&mut self, c: char) -> Option<u16> {
		use x11rb::connection::Connection as _;
		use x11rb::protocol::xproto::ConnectionExt as _;
		let sym = super::window_common::char_to_keysym(c)?;
		let (kc, mapped) = self.slots.assign(c)?;
		if mapped {
			return Some(kc as u16 - 8);
		}
		let per = self.syms_per.max(1);
		let mut syms = vec![0u32; per];
		syms[0] = sym;
		if per > 1 {
			syms[1] = sym;
		}
		// Checked: the server must have applied the mapping before our key press reaches it.
		let applied = self.conn.change_keyboard_mapping(1, kc, per as u8, &syms).ok().and_then(|c| c.check().ok());
		let _ = self.conn.flush();
		if applied.is_none() {
			self.slots.forget(kc);
			return None;
		}
		Some(kc as u16 - 8)
	}
}

impl Drop for X11Scratch {
	fn drop(&mut self) {
		use x11rb::connection::Connection as _;
		use x11rb::protocol::xproto::ConnectionExt as _;
		let per = self.syms_per.max(1);
		for kc in self.slots.mapped() {
			let _ = self.conn.change_keyboard_mapping(1, kc, per as u8, &vec![0u32; per]);
		}
		let _ = self.conn.flush();
	}
}
