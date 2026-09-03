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

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::WindowIdentifier;
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
	session: Option<Session<'static, Screencast<'static>>>,
	_pw_fd: OwnedFd,
	/// Set when the bus reported an error / EOS (the pipeline is dead).
	dead: Arc<AtomicBool>,
	/// Short-GOP recovery: a timer forces a key unit every 500 ms while set.
	short_gop: Arc<AtomicBool>,
	/// Set on stop/drop so the helper tasks exit.
	stopped: Arc<AtomicBool>,
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
struct SessionCloseGuard(Option<Session<'static, Screencast<'static>>>);

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
	let (mut guard, node_id, pw_fd, token) = open_portal(restore_token).await?;

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
) -> anyhow::Result<(SessionCloseGuard, u32, OwnedFd, Option<String>)> {
	let proxy: Screencast<'static> = Screencast::new().await?;
	let session: Session<'static, Screencast<'static>> = proxy.create_session().await?;
	let guard = SessionCloseGuard(Some(session));
	let (node_id, pw_fd, token) = async {
		let session = guard.0.as_ref().expect("session present");
		proxy
			.select_sources(
				session,
				CursorMode::Embedded,
				SourceType::Monitor | SourceType::Window,
				false,
				restore_token.as_deref(),
				PersistMode::Application,
			)
			.await?;
		let response = proxy
			.start(session, &WindowIdentifier::default())
			.await?
			.response()?;
		let stream = response
			.streams()
			.first()
			.ok_or_else(|| anyhow::anyhow!("portal returned no screencast stream"))?;
		let node_id = stream.pipe_wire_node_id();
		let token = response.restore_token().map(|s| s.to_string());

		let pw_fd: OwnedFd = proxy.open_pipe_wire_remote(session).await?;
		clear_cloexec(pw_fd.as_raw_fd())?;
		Ok::<_, anyhow::Error>((node_id, pw_fd, token))
	}
	.await?;
	Ok((guard, node_id, pw_fd, token))
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
