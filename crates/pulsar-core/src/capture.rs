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
//!
//! Fallback: when the in-process GStreamer lacks an element of the pipeline (an AppImage
//! whose bundled 1.20 core can't load the distro's newer plugins), the same description
//! runs under the SYSTEM `gst-launch-1.0` instead — no live bitrate/keyframe control then,
//! but a picture instead of a black screen. The caller can also force it (`system_gst`):
//! the element exists in-process but can't open the hardware (the AppImage's 1.20 NVENC
//! plugin on a newer NVIDIA driver) while the system plugin can.

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
	backend: Backend,
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

/// Where the pipeline runs: in this process (live controls), or as a system
/// `gst-launch-1.0` child (fallback, see the module docs).
enum Backend {
	InProcess {
		pipeline: gst::Pipeline,
		venc: Option<gst::Element>,
	},
	Child(std::sync::Mutex<std::process::Child>),
}

impl Backend {
	fn venc(&self) -> Option<&gst::Element> {
		match self {
			Backend::InProcess { venc, .. } => venc.as_ref(),
			Backend::Child(_) => None,
		}
	}

	/// Tear the pipeline down. Runs BEFORE the portal session closes (see `gst.rs`).
	fn shutdown(&self) {
		match self {
			Backend::InProcess { pipeline, .. } => {
				let _ = pipeline.set_state(gst::State::Null);
			}
			Backend::Child(child) => {
				let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
				let _ = child.kill();
				let _ = child.wait();
			}
		}
	}
}

impl WaylandCapture {
	/// Whether the pipeline is still running (no bus error / EOS seen, child not exited).
	pub fn is_alive(&self) -> bool {
		match &self.backend {
			Backend::InProcess { .. } => !self.dead.load(Ordering::Relaxed),
			Backend::Child(child) => matches!(
				child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(),
				Ok(None)
			),
		}
	}

	/// Whether the encoder can be driven live (`false` on the `gst-launch-1.0` fallback:
	/// a bitrate change then needs a pipeline restart).
	pub fn is_in_process(&self) -> bool {
		matches!(self.backend, Backend::InProcess { .. })
	}

	/// Change the encoder's target bitrate (kbit/s) **live** — no restart. Handles the
	/// property vocabulary of the elements `pipeline::gst::encoder_fragment` can emit
	/// (`bitrate` in kbit/s on x264enc / vaapih26xenc / nvh26xenc, `bps` in bit/s on the
	/// Rockchip mpp encoders). `false` when the element has no such property.
	pub fn set_bitrate(&self, kbps: u32) -> bool {
		let Some(enc) = self.backend.venc() else { return false };
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
		let Some(enc) = self.backend.venc() else { return false };
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
		self.backend.shutdown();
		if let Some(session) = self.session.take() {
			let _ = session.close().await;
		}
	}
}

impl Drop for WaylandCapture {
	fn drop(&mut self) {
		self.stopped.store(true, Ordering::Relaxed);
		self.backend.shutdown();
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
/// skipped after the first time), then run the pipeline in-process — or under the system
/// `gst-launch-1.0` when `system_gst` is set (the caller validated the encoder only there).
/// Returns the capture and the (possibly new) restore token to persist.
pub async fn start(
	ip: &str,
	port: u16,
	encoder_fragment: &str,
	restore_token: Option<String>,
	system_gst: bool,
) -> anyhow::Result<(WaylandCapture, Option<String>)> {
	// Can the in-process GStreamer build this pipeline at all? (The fd/node/address don't
	// change which elements it needs.) If not, run it under the system gst-launch-1.0.
	let in_process = !system_gst && match gst::init() {
		Ok(()) => {
			let missing = missing_elements(&crate::pipeline::gst::wayland_pipeline(
				0,
				0,
				encoder_fragment,
				ip,
				port,
			));
			if !missing.is_empty() {
				tracing::warn!(?missing, "in-process GStreamer lacks elements — using the system gst-launch-1.0");
			}
			missing.is_empty()
		}
		Err(e) => {
			tracing::warn!("in-process GStreamer init failed ({e}) — using the system gst-launch-1.0");
			false
		}
	};
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
		let started = if in_process {
			run_pipeline(pw_fd.as_raw_fd(), node_id, encoder_fragment, ip, port)
				.await
				.map(|(pipeline, venc)| Backend::InProcess { pipeline, venc })
		} else {
			run_child(pw_fd.as_raw_fd(), node_id, encoder_fragment, ip, port)
				.await
				.map(|child| Backend::Child(std::sync::Mutex::new(child)))
		};
		match started {
			Ok(backend) => {
				let session = guard.0.take().expect("session present on success");
				let dead = Arc::new(AtomicBool::new(false));
				let short_gop = Arc::new(AtomicBool::new(false));
				let stopped = Arc::new(AtomicBool::new(false));
				if let Backend::InProcess { pipeline, venc } = &backend {
					spawn_watchers(pipeline, venc.clone(), dead.clone(), short_gop.clone(), stopped.clone());
				}
				return Ok((
					WaylandCapture {
						backend,
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
				tracing::warn!(attempt, in_process, "gst pipeline start failed, retrying: {e}");
				last_err = Some(e);
			}
		}
	}
	Err(last_err.unwrap_or_else(|| anyhow::anyhow!("wayland capture start failed")))
}

/// Factory names of a `gst-launch` description that the in-process GStreamer can't make
/// (the first word of each `!` segment; caps segments contain a `/` and are skipped).
fn missing_elements(desc: &str) -> Vec<String> {
	desc.split('!')
		.filter_map(|seg| seg.split_whitespace().next())
		.filter(|name| !name.contains('/'))
		.filter(|name| gst::ElementFactory::find(name).is_none())
		.map(str::to_owned)
		.collect()
}

/// Headless check of the IN-PROCESS GStreamer for one encoder fragment (`pulsar --selftest`,
/// CI's AppImage smoke test): every element of the Wayland pipeline must be loadable, and a
/// short `videotestsrc` encode through `encoder_fragment` must reach EOS — which is what
/// actually opens a hardware encoder (VA-API driver, NVENC) rather than just its factory.
pub fn self_check(encoder_fragment: &str) -> Result<(), String> {
	gst::init().map_err(|e| format!("gst init: {e}"))?;
	let full = crate::pipeline::gst::wayland_pipeline(0, 0, encoder_fragment, "127.0.0.1", 9);
	let missing = missing_elements(&full);
	if !missing.is_empty() {
		return Err(format!("missing elements: {}", missing.join(", ")));
	}
	let desc = format!(
		"videotestsrc num-buffers=30 ! video/x-raw,width=640,height=360,framerate=30/1 \
		 ! videoconvert ! video/x-raw,format=NV12 ! {encoder_fragment} ! fakesink sync=false"
	);
	let pipeline = gst::parse::launch(&desc)
		.map_err(|e| format!("parse: {e}"))?
		.downcast::<gst::Pipeline>()
		.map_err(|_| "not a pipeline".to_string())?;
	let bus = pipeline.bus().ok_or("no bus")?;
	let res = match pipeline.set_state(gst::State::Playing) {
		Err(e) => Err(format!("start: {e}")),
		Ok(_) => match bus.timed_pop_filtered(
			gst::ClockTime::from_seconds(15),
			&[gst::MessageType::Eos, gst::MessageType::Error],
		) {
			None => Err("timed out".into()),
			Some(m) => match m.view() {
				gst::MessageView::Error(e) => Err(format!("{}", e.error())),
				_ => Ok(()),
			},
		},
	};
	let _ = pipeline.set_state(gst::State::Null);
	res
}

/// File-name prefix of the AppImage's own GStreamer registry (`GST_REGISTRY_1_0`, set by the
/// desktop app at startup): it lives outside `$APPDIR`, so [`use_system_gst_env`] matches it
/// by name.
pub const APPIMAGE_REGISTRY_PREFIX: &str = "registry.appimage.";

/// Environment variables an AppImage points at its bundled GStreamer.
const BUNDLED_ENV: &[&str] = &[
	"GST_PLUGIN_SYSTEM_PATH",
	"GST_PLUGIN_SYSTEM_PATH_1_0",
	"GST_PLUGIN_PATH",
	"GST_PLUGIN_PATH_1_0",
	"GST_PLUGIN_SCANNER",
	"GST_PLUGIN_SCANNER_1_0",
	"GST_PTP_HELPER",
	"GST_PTP_HELPER_1_0",
	"GST_REGISTRY",
	"GST_REGISTRY_1_0",
];

/// Make `cmd` (a SYSTEM gst tool) see the system's own plugins: drop every
/// variable from [`BUNDLED_ENV`] that points into the running AppImage (`$APPDIR`, or its
/// registry file, [`APPIMAGE_REGISTRY_PREFIX`]) and `$APPDIR` entries of `LD_LIBRARY_PATH`/`PATH`. The
/// system 1.2x tools can't load the AppImage's 1.20 plugins and vice versa. Variables the
/// user set themselves (not under `$APPDIR`) are kept; a no-op outside an AppImage.
pub fn use_system_gst_env(cmd: &mut std::process::Command) {
	let Some(appdir) = std::env::var_os("APPDIR").filter(|d| !d.is_empty()) else { return };
	let appdir = appdir.to_string_lossy().into_owned();
	for key in BUNDLED_ENV {
		if std::env::var(key).is_ok_and(|v| v.contains(&appdir) || v.contains(APPIMAGE_REGISTRY_PREFIX)) {
			cmd.env_remove(key);
		}
	}
	// linuxdeploy's hook sets this one to a bare "no" (no path to match): drop it too.
	if std::env::var("GST_REGISTRY_REUSE_PLUGIN_SCANNER").is_ok_and(|v| v == "no") {
		cmd.env_remove("GST_REGISTRY_REUSE_PLUGIN_SCANNER");
	}
	// The AppImage's AppRun puts `$APPDIR/usr/lib…` on LD_LIBRARY_PATH (a system tool would
	// load the bundled libgstreamer 1.20 and die) and `$APPDIR/usr/bin` first on PATH (a bare
	// `gst-launch-1.0` would BE the bundled one — std resolves the program with the PATH set
	// here).
	for key in ["LD_LIBRARY_PATH", "PATH"] {
		let Ok(cur) = std::env::var(key) else { continue };
		let kept: Vec<&str> = cur
			.split(':')
			.filter(|d| !d.is_empty() && !d.starts_with(&appdir))
			.collect();
		if kept.is_empty() {
			cmd.env_remove(key);
		} else {
			cmd.env(key, kept.join(":"));
		}
	}
}

/// Run the pipeline under the system `gst-launch-1.0` (the in-process GStreamer can't).
/// The PipeWire fd is inherited (its CLOEXEC is cleared in `open_portal`). Up once the
/// child is still alive after a short grace period; its stderr goes to the log.
async fn run_child(
	fd: i32,
	node_id: u32,
	encoder_fragment: &str,
	ip: &str,
	port: u16,
) -> anyhow::Result<std::process::Child> {
	use std::io::BufRead;
	use std::os::unix::process::CommandExt;
	let desc = crate::pipeline::gst::wayland_pipeline(fd, node_id, encoder_fragment, ip, port);
	let mut cmd = std::process::Command::new("gst-launch-1.0");
	cmd.arg("-q")
		.args(desc.split_whitespace())
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::piped());
	use_system_gst_env(&mut cmd);
	// Die with the app instead of streaming on as an orphan.
	unsafe {
		cmd.pre_exec(|| {
			// SAFETY: async-signal-safe libc call only.
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
			Ok(())
		});
	}
	let mut child = cmd
		.spawn()
		.map_err(|e| anyhow::anyhow!("gst-launch-1.0 başlatılamadı (GStreamer kurulu mu?): {e}"))?;
	if let Some(stderr) = child.stderr.take() {
		std::thread::spawn(move || {
			for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
				tracing::warn!("gst-launch-1.0: {line}");
			}
		});
	}
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	match child.try_wait() {
		Ok(None) => Ok(child),
		Ok(Some(status)) => Err(anyhow::anyhow!(
			"gst-launch-1.0 kapandı ({status}) — pipewire/x264/rtp GStreamer eklentileri kurulu mu?"
		)),
		Err(e) => Err(anyhow::anyhow!("gst-launch-1.0 durumu okunamadı: {e}")),
	}
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

	#[test]
	fn missing_elements_names_unknown_factories_and_skips_caps() {
		gst::init().unwrap();
		let desc = "videotestsrc ! video/x-raw,format=NV12 ! pulsar-no-such-element name=x ! fakesink sync=false";
		assert_eq!(missing_elements(desc), vec!["pulsar-no-such-element".to_string()]);
		assert!(missing_elements("videotestsrc num-buffers=1 ! fakesink").is_empty());
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
