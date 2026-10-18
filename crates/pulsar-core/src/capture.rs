//! Wayland screen capture for the host: XDG ScreenCast portal → PipeWire → an **in-process
//! GStreamer pipeline** (`pipewiresrc ! … ! encoder ! rtp payloader ! udpsink`).
//!
//! Why in-process (adaptive streaming, 2026-09-03): the old `gst-launch` child could not be
//! touched while running — every bitrate/GOP change meant killing it and re-opening the
//! portal, which on KDE regularly ended in a black screen (the new pipeline never reached
//! PAUSED while KWin was still tearing the old cast down). With the pipeline inside the
//! app the encoder's bitrate changes live, a keyframe can be forced on request, and a
//! short-GOP recovery mode is a timer that forces key units — no restart, no portal churn.
//!
//! Linux-only (`cfg`); `x11grab` of a rootless Xwayland is always black, so this is the
//! only way to stream a Wayland desktop.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Are we on a Wayland session (so `x11grab` would be black)?
pub fn is_wayland() -> bool {
	std::env::var("XDG_SESSION_TYPE")
		.map(|v| v.eq_ignore_ascii_case("wayland"))
		.unwrap_or(false)
		|| std::env::var("WAYLAND_DISPLAY")
			.map(|v| !v.is_empty())
			.unwrap_or(false)
}

/// Name the encoder element carries inside the pipeline (`… name=venc …`), so the live
/// controls can find it.
pub const ENCODER_NAME: &str = "venc";

/// A running Wayland capture: the GStreamer pipeline + the portal session that feeds it.
/// Dropping it (or `stop()`) tears the pipeline down AND closes the portal session — the
/// session lingers otherwise and PipeWire may re-link a stale source to a webcam.
pub struct WaylandCapture {
	pipeline: gst::Pipeline,
	venc: Option<gst::Element>,
	session: Option<Session<Screencast>>,
	_pw_fd: OwnedFd,
	/// Set when the bus reported an error / EOS (the pipeline is dead).
	dead: Arc<AtomicBool>,
	/// Short-GOP recovery: a timer forces a key unit every 500 ms while set.
	short_gop: Arc<AtomicBool>,
	/// Set on stop/drop so the helper tasks exit.
	stopped: Arc<AtomicBool>,
	/// Where the streamed monitor sits in the output layout, see [`Self::pointer_region`].
	pointer_region: Option<PointerRegion>,
}

/// The streamed monitor's rectangle as fractions (0..1) of the bounding box of all outputs
/// — the area the compositor maps the absolute uinput pointer onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerRegion {
	pub x: f64,
	pub y: f64,
	pub w: f64,
	pub h: f64,
}

impl WaylandCapture {
	/// Whether the pipeline is still running (no bus error / EOS seen).
	pub fn is_alive(&self) -> bool {
		!self.dead.load(Ordering::Relaxed)
	}

	/// Change the encoder's target bitrate (kbit/s) **live** — no restart. Handles the
	/// property vocabulary of the elements `pipeline::gst::encoder_fragment` can emit
	/// (`bitrate` in kbit/s on x264enc / vaapih26xenc / nvh26xenc, `bps` in bit/s on the
	/// Rockchip mpp encoders). `false` when the element has no such property.
	pub fn set_bitrate(&self, kbps: u32) -> bool {
		let Some(enc) = self.venc.as_ref() else { return false };
		if enc.find_property("bitrate").is_some() {
			enc.set_property_from_str("bitrate", &kbps.max(1).to_string());
			true
		} else if enc.find_property("bps").is_some() {
			enc.set_property_from_str("bps", &kbps.max(1).saturating_mul(1000).to_string());
			true
		} else {
			false
		}
	}

	/// Force the next frame to be a keyframe (a client keyframe request after an
	/// unrepaired loss) — an upstream force-key-unit event into the encoder.
	pub fn request_keyframe(&self) -> bool {
		let Some(enc) = self.venc.as_ref() else { return false };
		let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
			.all_headers(true)
			.build();
		enc.send_event(ev)
	}

	/// Loss-recovery mode: `true` = force a key unit every ~0.5 s (the client asked for the
	/// short GOP / intra refresh; gst encoders can't change their GOP live, so the keyframes
	/// are forced instead), `false` = the element's own key interval.
	pub fn set_short_gop(&self, on: bool) {
		self.short_gop.store(on, Ordering::Relaxed);
	}

	/// The streamed monitor within the output layout, for
	/// [`crate::input::DesktopInput::set_region`]: without it a client's 0..1 pointer spans
	/// every monitor on a multi-monitor host. `None` for a window cast or when the portal /
	/// compositor gives no geometry (then the pointer covers the whole layout, as before).
	pub fn pointer_region(&self) -> Option<PointerRegion> {
		self.pointer_region
	}

	/// Stop the capture: tear the pipeline down and **close the portal session**.
	pub async fn stop(mut self) {
		self.stopped.store(true, Ordering::Relaxed);
		let _ = self.pipeline.set_state(gst::State::Null);
		if let Some(session) = self.session.take() {
			let _ = session.close().await;
		}
	}
}

impl Drop for WaylandCapture {
	fn drop(&mut self) {
		self.stopped.store(true, Ordering::Relaxed);
		let _ = self.pipeline.set_state(gst::State::Null);
		if let Some(session) = self.session.take() {
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.spawn(async move {
					let _ = session.close().await;
				});
			}
		}
	}
}

fn clear_cloexec(fd: i32) -> std::io::Result<()> {
	let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
	if flags < 0 {
		return Err(std::io::Error::last_os_error());
	}
	if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
		return Err(std::io::Error::last_os_error());
	}
	Ok(())
}

/// Closes the portal session if `start` bails out before the capture owns it.
struct SessionCloseGuard(Option<Session<Screencast>>);

impl Drop for SessionCloseGuard {
	fn drop(&mut self) {
		if let Some(session) = self.0.take() {
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.spawn(async move {
					let _ = session.close().await;
				});
			}
		}
	}
}

/// Open the portal (with the persisted restore token, if any, so the share dialog is
/// skipped after the first time), then run the pipeline in-process. Returns the capture
/// and the (possibly new) restore token to persist.
pub async fn start(
	ip: &str,
	port: u16,
	encoder_fragment: &str,
	restore_token: Option<String>,
) -> anyhow::Result<(WaylandCapture, Option<String>)> {
	gst::init().map_err(|e| anyhow::anyhow!("GStreamer başlatılamadı: {e}"))?;
	let (mut guard, node_id, pw_fd, token, stream_rect) = open_portal(restore_token).await?;
	let pointer_region = stream_rect.and_then(|r| {
		let region = outputs::bounds().and_then(|b| region_in(r, b));
		tracing::info!(?r, ?region, "wayland pointer region");
		region
	});

	// The PipeWire node can lag the portal reply by a moment: retry the pipeline start a
	// few times before giving up (each attempt tears its pipeline down cleanly).
	let mut last_err: Option<anyhow::Error> = None;
	for attempt in 0u32..3 {
		if attempt > 0 {
			tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
		}
		match run_pipeline(pw_fd.as_raw_fd(), node_id, encoder_fragment, ip, port).await {
			Ok((pipeline, venc)) => {
				let session = guard.0.take().expect("session present on success");
				let dead = Arc::new(AtomicBool::new(false));
				let short_gop = Arc::new(AtomicBool::new(false));
				let stopped = Arc::new(AtomicBool::new(false));
				spawn_watchers(&pipeline, venc.clone(), dead.clone(), short_gop.clone(), stopped.clone());
				return Ok((
					WaylandCapture {
						pipeline,
						venc,
						session: Some(session),
						_pw_fd: pw_fd,
						dead,
						short_gop,
						stopped,
						pointer_region,
					},
					token,
				));
			}
			Err(e) => {
				tracing::warn!(attempt, "gst pipeline start failed, retrying: {e}");
				last_err = Some(e);
			}
		}
	}
	Err(last_err.unwrap_or_else(|| anyhow::anyhow!("wayland capture start failed")))
}

async fn open_portal(
	restore_token: Option<String>,
) -> anyhow::Result<(SessionCloseGuard, u32, OwnedFd, Option<String>, Option<Rect>)> {
	let proxy: Screencast = Screencast::new().await?;
	let session: Session<Screencast> = proxy.create_session(Default::default()).await?;
	let guard = SessionCloseGuard(Some(session));
	let (node_id, pw_fd, token, rect) = async {
		let session = guard.0.as_ref().expect("session present");
		proxy
			.select_sources(
				session,
				SelectSourcesOptions::default()
					.set_cursor_mode(CursorMode::Embedded)
					.set_sources(SourceType::Monitor | SourceType::Window)
					.set_multiple(false)
					.set_restore_token(restore_token.as_deref())
					.set_persist_mode(PersistMode::Application),
			)
			.await?;
		let response = proxy
			.start(session, None, Default::default())
			.await?
			.response()?;
		let stream = response
			.streams()
			.first()
			.ok_or_else(|| anyhow::anyhow!("portal returned no screencast stream"))?;
		let node_id = stream.pipe_wire_node_id();
		// Monitor casts carry the output's place in the compositor's logical layout.
		let rect = match (stream.position(), stream.size()) {
			(Some((x, y)), Some((w, h))) if w > 0 && h > 0 => Some(Rect { x, y, w, h }),
			_ => None,
		};
		let token = response.restore_token().map(|s| s.to_string());

		let pw_fd: OwnedFd = proxy
			.open_pipe_wire_remote(session, Default::default())
			.await?;
		clear_cloexec(pw_fd.as_raw_fd())?;
		Ok::<_, anyhow::Error>((node_id, pw_fd, token, rect))
	}
	.await?;
	Ok((guard, node_id, pw_fd, token, rect))
}

/// A rectangle in the compositor's logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
	x: i32,
	y: i32,
	w: i32,
	h: i32,
}

/// `r` as fractions of `bounds`; `None` unless `r` lies inside `bounds`.
fn region_in(r: Rect, bounds: Rect) -> Option<PointerRegion> {
	if bounds.w <= 0
		|| bounds.h <= 0
		|| r.x < bounds.x
		|| r.y < bounds.y
		|| r.x + r.w > bounds.x + bounds.w
		|| r.y + r.h > bounds.y + bounds.h
	{
		return None;
	}
	// Pixel centres of the first and last pixel, so 1.0 lands on the monitor's last pixel
	// and not on the first pixel of its right/bottom neighbour.
	let (bw, bh) = (bounds.w as f64, bounds.h as f64);
	Some(PointerRegion {
		x: ((r.x - bounds.x) as f64 + 0.5) / bw,
		y: ((r.y - bounds.y) as f64 + 0.5) / bh,
		w: (r.w - 1).max(0) as f64 / bw,
		h: (r.h - 1).max(0) as f64 / bh,
	})
}

/// The output layout via xdg-output (logical position + size per output, the same space
/// the portal reports the stream in).
mod outputs {
	use super::Rect;
	use wayland_client::protocol::{wl_output, wl_registry};
	use wayland_client::{Connection, Dispatch, QueueHandle};
	use wayland_protocols::xdg::xdg_output::zv1::client::{
		zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1, zxdg_output_v1::ZxdgOutputV1,
	};

	#[derive(Default)]
	struct State {
		outputs: Vec<wl_output::WlOutput>,
		manager: Option<ZxdgOutputManagerV1>,
		rects: Vec<Rect>,
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
				match interface.as_str() {
					"wl_output" => state.outputs.push(registry.bind(name, version.min(4), qh, ())),
					"zxdg_output_manager_v1" => state.manager = Some(registry.bind(name, version.min(3), qh, ())),
					_ => {}
				}
			}
		}
	}

	impl Dispatch<wl_output::WlOutput, ()> for State {
		fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
	}

	impl Dispatch<ZxdgOutputManagerV1, ()> for State {
		fn event(
			_: &mut Self,
			_: &ZxdgOutputManagerV1,
			_: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
			_: &(),
			_: &Connection,
			_: &QueueHandle<Self>,
		) {
		}
	}

	impl Dispatch<ZxdgOutputV1, usize> for State {
		fn event(
			state: &mut Self,
			_: &ZxdgOutputV1,
			event: zxdg_output_v1::Event,
			idx: &usize,
			_: &Connection,
			_: &QueueHandle<Self>,
		) {
			let Some(r) = state.rects.get_mut(*idx) else { return };
			match event {
				zxdg_output_v1::Event::LogicalPosition { x, y } => (r.x, r.y) = (x, y),
				zxdg_output_v1::Event::LogicalSize { width, height } => (r.w, r.h) = (width, height),
				_ => {}
			}
		}
	}

	/// Bounding box of all outputs, `None` without a Wayland connection / xdg-output.
	pub(super) fn bounds() -> Option<Rect> {
		let conn = Connection::connect_to_env().ok()?;
		let mut queue = conn.new_event_queue();
		let qh = queue.handle();
		conn.display().get_registry(&qh, ());
		let mut state = State::default();
		queue.roundtrip(&mut state).ok()?;
		let manager = state.manager.clone()?;
		state.rects = vec![Rect { x: 0, y: 0, w: 0, h: 0 }; state.outputs.len()];
		for (i, o) in state.outputs.clone().iter().enumerate() {
			manager.get_xdg_output(o, &qh, i);
		}
		queue.roundtrip(&mut state).ok()?;
		let rects: Vec<Rect> = state.rects.into_iter().filter(|r| r.w > 0 && r.h > 0).collect();
		let x0 = rects.iter().map(|r| r.x).min()?;
		let y0 = rects.iter().map(|r| r.y).min()?;
		let x1 = rects.iter().map(|r| r.x + r.w).max()?;
		let y1 = rects.iter().map(|r| r.y + r.h).max()?;
		Some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn region_of_one_monitor_in_a_three_monitor_layout() {
		// 2560x1440 + 2560x1440 side by side, 1920x1200 below the first.
		let bounds = Rect { x: 0, y: 0, w: 5120, h: 2640 };
		let r = region_in(Rect { x: 2560, y: 0, w: 2560, h: 1440 }, bounds).unwrap();
		let px = |f: f64, size: f64| (f * size).floor();
		// 0 → the monitor's first pixel, 1 → its last pixel (never the neighbour's first).
		assert_eq!((px(r.x, 5120.0), px(r.y, 2640.0)), (2560.0, 0.0));
		assert_eq!((px(r.x + r.w, 5120.0), px(r.y + r.h, 2640.0)), (5119.0, 1439.0));
		let left = region_in(Rect { x: 0, y: 0, w: 2560, h: 1440 }, bounds).unwrap();
		assert_eq!(px(left.x + left.w, 5120.0), 2559.0);
		assert!(region_in(Rect { x: 4000, y: 0, w: 2560, h: 1440 }, bounds).is_none());
	}

	/// Needs a live Wayland session: `cargo test -p pulsar-core --lib live_output_bounds -- --ignored --nocapture`.
	#[test]
	#[ignore]
	fn live_output_bounds() {
		println!("output bounds: {:?}", outputs::bounds());
	}
}

/// Build the pipeline from the same description string the `gst-launch` path used, set it
/// PLAYING and confirm it really got there (a PipeWire node that isn't ready fails the
/// state change instead of streaming). Returns the pipeline + the encoder element.
async fn run_pipeline(
	fd: i32,
	node_id: u32,
	encoder_fragment: &str,
	ip: &str,
	port: u16,
) -> anyhow::Result<(gst::Pipeline, Option<gst::Element>)> {
	let desc = crate::pipeline::gst::wayland_pipeline(fd, node_id, encoder_fragment, ip, port);
	let element = gst::parse::launch(&desc)
		.map_err(|e| anyhow::anyhow!("gst pipeline parse failed (gstreamer plugins kurulu mu?): {e}"))?;
	let pipeline = element
		.downcast::<gst::Pipeline>()
		.map_err(|_| anyhow::anyhow!("gst pipeline description did not build a pipeline"))?;
	let venc = pipeline.by_name(ENCODER_NAME);
	if venc.is_none() {
		tracing::warn!("wayland pipeline has no `{ENCODER_NAME}` element — live bitrate/keyframe controls disabled");
	}
	pipeline
		.set_state(gst::State::Playing)
		.map_err(|e| anyhow::anyhow!("gst pipeline could not start: {e}"))?;
	// The state change is async (PipeWire has to link the node): wait for PLAYING, bounded.
	let (res, cur, _pending) = pipeline.state(gst::ClockTime::from_seconds(3));
	if res.is_err() || cur != gst::State::Playing {
		let msg = pipeline
			.bus()
			.and_then(|b| b.pop_filtered(&[gst::MessageType::Error]))
			.and_then(|m| match m.view() {
				gst::MessageView::Error(e) => Some(format!("{}", e.error())),
				_ => None,
			})
			.unwrap_or_else(|| format!("state {cur:?}"));
		let _ = pipeline.set_state(gst::State::Null);
		return Err(anyhow::anyhow!(
			"gst pipeline failed to reach PLAYING (pipewire/portal node not ready?): {msg}"
		));
	}
	Ok((pipeline, venc))
}

/// Helper tasks for a running pipeline: (1) drain the bus and flag errors / EOS, (2) the
/// short-GOP timer that forces a key unit every 500 ms while recovery mode is on. Both
/// exit when the capture is stopped.
fn spawn_watchers(
	pipeline: &gst::Pipeline,
	venc: Option<gst::Element>,
	dead: Arc<AtomicBool>,
	short_gop: Arc<AtomicBool>,
	stopped: Arc<AtomicBool>,
) {
	let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
	if let Some(bus) = pipeline.bus() {
		let dead = dead.clone();
		let stopped = stopped.clone();
		handle.spawn(async move {
			while !stopped.load(Ordering::Relaxed) {
				while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
					match msg.view() {
						gst::MessageView::Error(e) => {
							tracing::error!(
								src = ?e.src().map(|s| s.path_string()),
								"wayland gst pipeline error: {} ({:?})",
								e.error(),
								e.debug()
							);
							dead.store(true, Ordering::Relaxed);
						}
						gst::MessageView::Eos(_) => {
							tracing::warn!("wayland gst pipeline reached EOS");
							dead.store(true, Ordering::Relaxed);
						}
						_ => {}
					}
				}
				tokio::time::sleep(std::time::Duration::from_millis(250)).await;
			}
		});
	}
	if let Some(enc) = venc {
		handle.spawn(async move {
			let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
			tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			while !stopped.load(Ordering::Relaxed) {
				tick.tick().await;
				if short_gop.load(Ordering::Relaxed) && !dead.load(Ordering::Relaxed) {
					let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
						.all_headers(true)
						.build();
					let _ = enc.send_event(ev);
				}
			}
		});
	}
}
