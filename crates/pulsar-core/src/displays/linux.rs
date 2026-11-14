//! Linux monitors: Wayland outputs (`wl_output` name/description/current mode +
//! `zxdg_output_v1` logical layout) over a short-lived Wayland connection, and change
//! events from a long-lived one (Wayland) or from X11 RandR. Untested on Wayland / X11
//! multi-monitor hardware.

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{
	zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1, zxdg_output_v1::ZxdgOutputV1,
};

use super::{normalize_modes, order_primary_first, Monitor};

/// Everything one output told us.
#[derive(Debug, Default, Clone)]
struct Out {
	global: u32,
	connector: String,
	description: String,
	make: String,
	model: String,
	/// Current mode, pixels.
	mode: (u32, u32),
	refresh_mhz: i32,
	/// All advertised modes (wl_output v1-3 only; v4 compositors usually send the current one).
	modes: Vec<(u32, u32)>,
	/// wl_output geometry position (compositor space; logical on most compositors).
	geo_pos: (i32, i32),
	scale: i32,
	logical_pos: Option<(i32, i32)>,
	logical_size: Option<(i32, i32)>,
	xdg_name: String,
	xdg_description: String,
}

#[derive(Default)]
struct ListState {
	outputs: Vec<(wl_output::WlOutput, Out)>,
	manager: Option<ZxdgOutputManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ListState {
	fn event(
		state: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		qh: &QueueHandle<Self>,
	) {
		if let wl_registry::Event::Global { name, interface, version } = event {
			match interface.as_str() {
				"wl_output" => {
					let idx = state.outputs.len();
					let o = registry.bind::<wl_output::WlOutput, usize, Self>(name, version.min(4), qh, idx);
					state.outputs.push((o, Out { global: name, scale: 1, ..Default::default() }));
				}
				"zxdg_output_manager_v1" => state.manager = Some(registry.bind(name, version.min(3), qh, ())),
				_ => {}
			}
		}
	}
}

/// Apply one `wl_output` event to `out`.
fn apply_output_event(out: &mut Out, event: wl_output::Event) {
	match event {
		wl_output::Event::Geometry { x, y, make, model, .. } => {
			out.geo_pos = (x, y);
			out.make = make;
			out.model = model;
		}
		wl_output::Event::Mode { flags, width, height, refresh } => {
			let wh = (width.max(0) as u32, height.max(0) as u32);
			out.modes.push(wh);
			let current = matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
			if current {
				out.mode = wh;
				out.refresh_mhz = refresh;
			}
		}
		wl_output::Event::Scale { factor } => out.scale = factor.max(1),
		wl_output::Event::Name { name } => out.connector = name,
		wl_output::Event::Description { description } => out.description = description,
		_ => {}
	}
}

impl Dispatch<wl_output::WlOutput, usize> for ListState {
	fn event(
		state: &mut Self,
		_: &wl_output::WlOutput,
		event: wl_output::Event,
		idx: &usize,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let Some((_, out)) = state.outputs.get_mut(*idx) {
			apply_output_event(out, event);
		}
	}
}

impl Dispatch<ZxdgOutputManagerV1, ()> for ListState {
	fn event(
		_: &mut Self,
		_: &ZxdgOutputManagerV1,
		_: <ZxdgOutputManagerV1 as Proxy>::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<ZxdgOutputV1, usize> for ListState {
	fn event(
		state: &mut Self,
		_: &ZxdgOutputV1,
		event: zxdg_output_v1::Event,
		idx: &usize,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let Some((_, out)) = state.outputs.get_mut(*idx) else { return };
		match event {
			zxdg_output_v1::Event::LogicalPosition { x, y } => out.logical_pos = Some((x, y)),
			zxdg_output_v1::Event::LogicalSize { width, height } => out.logical_size = Some((width, height)),
			zxdg_output_v1::Event::Name { name } => out.xdg_name = name,
			zxdg_output_v1::Event::Description { description } => out.xdg_description = description,
			_ => {}
		}
	}
}

/// Turn the raw output facts into a [`Monitor`] (pure — unit-tested).
fn to_monitor(o: &Out) -> Option<Monitor> {
	let (width, height) = if o.mode.0 > 0 && o.mode.1 > 0 {
		o.mode
	} else {
		let (w, h) = o.logical_size?;
		(w.max(0) as u32, h.max(0) as u32)
	};
	if width == 0 || height == 0 {
		return None;
	}
	let (x, y) = o.logical_pos.unwrap_or(o.geo_pos);
	// Without xdg-output: the mode divided by the integer scale (no fractional info).
	let (lw, lh) = match o.logical_size {
		Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
		_ => (width / o.scale.max(1) as u32, height / o.scale.max(1) as u32),
	};
	let connector = if !o.connector.is_empty() { o.connector.clone() } else { o.xdg_name.clone() };
	let description = if !o.description.is_empty() { o.description.clone() } else { o.xdg_description.clone() };
	let make_model = format!("{} {}", o.make.trim(), o.model.trim()).trim().to_string();
	let name = if !description.trim().is_empty() {
		description.trim().to_string()
	} else if !make_model.is_empty() && !connector.is_empty() {
		format!("{make_model} ({connector})")
	} else if !connector.is_empty() {
		connector
	} else if !make_model.is_empty() {
		make_model
	} else {
		// No name at all: the app labels it in the UI language.
		String::new()
	};
	let builtin = is_builtin_connector(&connector);
	Some(Monitor {
		name,
		x,
		y,
		logical_w: lw,
		logical_h: lh,
		width,
		height,
		primary: false,
		refresh_hz: if o.refresh_mhz > 0 { ((o.refresh_mhz as f64) / 1000.0).round() as u32 } else { 0 },
		modes: normalize_modes(o.modes.iter().copied()),
		id: o.global,
		builtin,
	})
}

/// A laptop / tablet's own panel connector (eDP-1, LVDS-1, DSI-1).
fn is_builtin_connector(connector: &str) -> bool {
	let c = connector.to_ascii_uppercase();
	c.starts_with("EDP") || c.starts_with("LVDS") || c.starts_with("DSI")
}

/// The Wayland outputs, primary (the one at the layout origin) first. Empty without a
/// Wayland connection.
pub(super) fn wayland_list() -> Vec<Monitor> {
	let Ok(conn) = Connection::connect_to_env() else {
		return Vec::new();
	};
	let mut queue = conn.new_event_queue();
	let qh = queue.handle();
	conn.display().get_registry(&qh, ());
	let mut state = ListState::default();
	// 1st roundtrip: the globals (outputs bound); 2nd: their events + xdg-output's.
	if queue.roundtrip(&mut state).is_err() {
		return Vec::new();
	}
	if let Some(manager) = state.manager.clone() {
		let outs: Vec<wl_output::WlOutput> = state.outputs.iter().map(|(o, _)| o.clone()).collect();
		for (i, o) in outs.iter().enumerate() {
			manager.get_xdg_output(o, &qh, i);
		}
	}
	if queue.roundtrip(&mut state).is_err() {
		return Vec::new();
	}
	let monitors: Vec<Monitor> = state.outputs.iter().filter_map(|(_, o)| to_monitor(o)).collect();
	order_primary_first(monitors)
}

// ---- change watching ----

type OnChange = Box<dyn Fn() + Send + Sync + 'static>;

/// A watcher thread is running (or starting). Cleared when it ends, so a later [`watch`]
/// call can start a fresh one after a lost connection.
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Never blocks the caller: the connection is set up on the watcher thread, which flips
/// [`super::watching`] once it listens.
pub(super) fn watch(on_change: OnChange) -> bool {
	use std::sync::atomic::Ordering;
	if STARTED.swap(true, Ordering::SeqCst) {
		return super::watching();
	}
	let wayland = crate::capture::is_wayland();
	let spawned = std::thread::Builder::new().name("display-watch".into()).spawn(move || {
		if wayland {
			wayland_watch(on_change);
		} else {
			x11_watch(on_change);
		}
		super::set_watching(false);
		STARTED.store(false, Ordering::SeqCst);
		tracing::warn!("display watch: event source ended — falling back to polling");
	});
	if spawned.is_err() {
		STARTED.store(false, Ordering::SeqCst);
		return false;
	}
	super::watching()
}

/// Long-lived Wayland connection: fires on an output added / removed and on every
/// `wl_output.done` after the initial burst (a mode, scale, position or name change).
struct WatchState {
	/// The bound outputs by global name — released again when the global goes away, so a
	/// long-lived connection does not collect one dead proxy per unplug.
	outputs: Vec<(u32, wl_output::WlOutput)>,
	initial: bool,
	on_change: OnChange,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WatchState {
	fn event(
		state: &mut Self,
		registry: &wl_registry::WlRegistry,
		event: wl_registry::Event,
		_: &(),
		_: &Connection,
		qh: &QueueHandle<Self>,
	) {
		match event {
			wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
				let o = registry.bind::<wl_output::WlOutput, u32, Self>(name, version.min(4), qh, name);
				state.outputs.push((name, o));
				if !state.initial {
					(state.on_change)();
				}
			}
			wl_registry::Event::GlobalRemove { name } => {
				if let Some(i) = state.outputs.iter().position(|(n, _)| *n == name) {
					let (_, o) = state.outputs.remove(i);
					// wl_output.release exists from v3; older proxies just drop client-side.
					if o.version() >= 3 {
						o.release();
					}
					(state.on_change)();
				}
			}
			_ => {}
		}
	}
}

impl Dispatch<wl_output::WlOutput, u32> for WatchState {
	fn event(
		state: &mut Self,
		_: &wl_output::WlOutput,
		event: wl_output::Event,
		_: &u32,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if matches!(event, wl_output::Event::Done) && !state.initial {
			(state.on_change)();
		}
	}
}

fn wayland_watch(on_change: OnChange) {
	let Ok(conn) = Connection::connect_to_env() else {
		return;
	};
	let mut queue = conn.new_event_queue();
	let qh = queue.handle();
	conn.display().get_registry(&qh, ());
	let mut state = WatchState { outputs: Vec::new(), initial: true, on_change };
	// Swallow the initial globals + output burst, then listen.
	if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
		return;
	}
	state.initial = false;
	super::set_watching(true);
	tracing::info!(outputs = state.outputs.len(), "display watch: listening for Wayland output changes");
	while queue.blocking_dispatch(&mut state).is_ok() {}
}

/// X11: RandR screen / CRTC / output change notifications on the root window.
fn x11_watch(on_change: OnChange) {
	use x11rb::connection::Connection as _;
	use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
	use x11rb::protocol::Event;
	let Ok((conn, screen)) = x11rb::connect(None) else {
		return;
	};
	let root = conn.setup().roots[screen].root;
	let armed = conn
		.randr_query_version(1, 2)
		.ok()
		.and_then(|c| c.reply().ok())
		.is_some()
		&& conn
			.randr_select_input(
				root,
				NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE,
			)
			.ok()
			.and_then(|c| c.check().ok())
			.is_some();
	if !armed {
		tracing::warn!("display watch: X11 RandR not available");
		return;
	}
	super::set_watching(true);
	tracing::info!("display watch: listening for X11 RandR changes");
	loop {
		match conn.wait_for_event() {
			Ok(Event::RandrScreenChangeNotify(_)) | Ok(Event::RandrNotify(_)) => on_change(),
			Ok(_) => {}
			Err(_) => return,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn output_events_build_a_monitor() {
		let mut o = Out { global: 7, scale: 1, ..Default::default() };
		apply_output_event(
			&mut o,
			wl_output::Event::Mode {
				flags: WEnum::Value(wl_output::Mode::Preferred),
				width: 1920,
				height: 1080,
				refresh: 60000,
			},
		);
		apply_output_event(
			&mut o,
			wl_output::Event::Mode {
				flags: WEnum::Value(wl_output::Mode::Current | wl_output::Mode::Preferred),
				width: 3840,
				height: 2160,
				refresh: 59940,
			},
		);
		apply_output_event(&mut o, wl_output::Event::Name { name: "DP-1".into() });
		o.logical_pos = Some((1920, 0));
		o.logical_size = Some((2560, 1440));
		let m = to_monitor(&o).unwrap();
		assert_eq!((m.width, m.height), (3840, 2160));
		assert_eq!((m.x, m.y, m.logical_w, m.logical_h), (1920, 0, 2560, 1440));
		assert_eq!(m.refresh_hz, 60);
		assert_eq!(m.name, "DP-1");
		assert_eq!(m.modes, vec![(3840, 2160), (1920, 1080)]);
		assert_eq!(m.id, 7);
	}

	#[test]
	fn description_wins_and_scale_gives_logical_size() {
		let mut o = Out { global: 3, scale: 2, ..Default::default() };
		o.mode = (2880, 1800);
		o.description = "Built-in display (eDP-1)".into();
		o.make = "BOE".into();
		let m = to_monitor(&o).unwrap();
		assert_eq!(m.name, "Built-in display (eDP-1)");
		assert_eq!((m.logical_w, m.logical_h), (1440, 900));
	}

	#[test]
	fn nameless_output_stays_unnamed_and_edp_is_builtin() {
		let mut o = Out { global: 9, scale: 1, ..Default::default() };
		o.mode = (1920, 1080);
		let m = to_monitor(&o).unwrap();
		assert_eq!(m.name, "");
		assert!(!m.builtin);
		o.xdg_name = "eDP-1".into();
		let m = to_monitor(&o).unwrap();
		assert_eq!(m.name, "eDP-1");
		assert!(m.builtin);
	}

	#[test]
	fn output_without_size_is_skipped() {
		assert!(to_monitor(&Out::default()).is_none());
	}
}
