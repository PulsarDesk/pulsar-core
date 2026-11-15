//! In-process GStreamer for a complete pipeline WITHOUT a portal: the X11 hosts whose hardware
//! encoder exists only as a GStreamer element — the RK3588 / Orange Pi 5 `ximagesrc ! … !
//! mpph26Xenc ! … ! udpsink` pipeline (`pipeline::gst::x11_pipeline` /
//! `x11_window_pipeline`).
//!
//! Why (2026-11-15): that pipeline ran as a `gst-launch-1.0` child, so the client's adaptive
//! bitrate steps were each a kill + respawn (a visible gap and a fresh IDR every couple of
//! seconds on a congested link, the gap itself read as more congestion) and a keyframe request
//! had no way in at all. Inside the app the same description gets the Wayland capture's live
//! controls: `bps` changes on the running encoder, a keyframe is an upstream force-key-unit
//! event, and the short-GOP recovery mode is a timer that forces key units.
//!
//! Plus an encode meter: pad probes on the encoder count the frames it puts out (the caller's
//! "no first frame" watchdog) and time each frame inside it ([`EncodeTimer`]) — the real
//! encode time, where the `gst-launch -v` pace meter could only time the gap between frames.
//!
//! The helper thread is a plain std thread (bus errors / EOS → dead; short-GOP key units), so
//! this works whether or not the caller runs inside a tokio runtime.
//!
//! NOT for `kmssrc` (the zero-copy KMS capture): reading the scanout framebuffer needs
//! `CAP_SYS_ADMIN`, which only the separate `~/pulsar-gst-launch` binary carries — that path
//! stays a child process (see the desktop's `host/handlers.rs`).
//!
//! Linux-only; untested on hardware (the Orange Pi 5 host) as of this change.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;

use super::{force_key_unit, missing_elements, parse_pipeline, play_pipeline, Backend, WaylandCapture};
use crate::pipeline::gst::EncodeTimer;

/// Called about once a second with the smoothed per-frame encode time (ms), from a GStreamer
/// streaming thread — keep it cheap and non-blocking.
pub type EncodeMsFn = Box<dyn Fn(f32) + Send + Sync + 'static>;

/// Run a complete pipeline description in this process (no portal, no PipeWire fd) and return
/// the same handle a Wayland capture gives: [`WaylandCapture::set_bitrate`],
/// [`WaylandCapture::request_keyframe`], [`WaylandCapture::set_short_gop`],
/// [`WaylandCapture::is_alive`], [`WaylandCapture::frames_encoded`]; dropping it (or `stop()`)
/// tears the pipeline down.
///
/// Fails — and the caller runs the description under `gst-launch-1.0` instead — when the
/// in-process GStreamer can't initialize, lacks an element of the description (an AppImage's
/// bundled GStreamer without the system's Rockchip plugin), or the pipeline can't reach
/// PLAYING within 3 s. Blocking (up to that 3 s); no tokio runtime needed.
pub fn start_pipeline(desc: &str, on_encode_ms: Option<EncodeMsFn>) -> anyhow::Result<WaylandCapture> {
	gst::init().map_err(|e| anyhow::anyhow!("in-process GStreamer init failed: {e}"))?;
	let missing = missing_elements(desc);
	if !missing.is_empty() {
		anyhow::bail!("in-process GStreamer lacks elements: {}", missing.join(", "));
	}
	let (pipeline, venc) = parse_pipeline(desc)?;
	let encoded = Arc::new(AtomicU64::new(0));
	// The meter goes on before PLAYING so the very first frame is counted.
	if let Some(enc) = &venc {
		attach_meter(enc, encoded.clone(), on_encode_ms);
	}
	play_pipeline(&pipeline)?;
	let dead = Arc::new(AtomicBool::new(false));
	let short_gop = Arc::new(AtomicBool::new(false));
	let stopped = Arc::new(AtomicBool::new(false));
	spawn_watch_thread(&pipeline, venc.clone(), dead.clone(), short_gop.clone(), stopped.clone());
	tracing::info!(live_controls = venc.is_some(), "in-process gst pipeline running (no portal)");
	Ok(WaylandCapture {
		backend: Backend::InProcess { pipeline, venc },
		session: None,
		_pw_fd: None,
		encoded,
		dead,
		short_gop,
		stopped,
		pointer_region: None,
		stream_rect: None,
	})
}

/// Pad probes on the encoder element: raw frames in (sink) and encoded frames out (src), timed
/// by PTS. Every buffer leaving the src pad counts as an encoded frame.
fn attach_meter(enc: &gst::Element, encoded: Arc<AtomicU64>, on_ms: Option<EncodeMsFn>) {
	let (Some(sink), Some(src)) = (enc.static_pad("sink"), enc.static_pad("src")) else {
		tracing::warn!("gst encoder has no static sink/src pad — no encode meter");
		return;
	};
	let timer = Arc::new(Mutex::new(EncodeTimer::default()));
	{
		let timer = timer.clone();
		sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
			if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
				timer
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.frame_in(pts.nseconds(), std::time::Instant::now());
			}
			gst::PadProbeReturn::Ok
		});
	}
	src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
		if let Some(buf) = info.buffer() {
			encoded.fetch_add(1, Ordering::Relaxed);
			if let Some(pts) = buf.pts() {
				let due = timer
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.frame_out(pts.nseconds(), std::time::Instant::now());
				if let (Some(ms), Some(f)) = (due, on_ms.as_ref()) {
					f(ms);
				}
			}
		}
		gst::PadProbeReturn::Ok
	});
}

/// One std thread per pipeline: drains the bus (error / EOS → `dead`) and, while `short_gop`
/// is set, forces a key unit every 500 ms. Exits within ~100 ms of `stopped`.
fn spawn_watch_thread(
	pipeline: &gst::Pipeline,
	venc: Option<gst::Element>,
	dead: Arc<AtomicBool>,
	short_gop: Arc<AtomicBool>,
	stopped: Arc<AtomicBool>,
) {
	let bus = pipeline.bus();
	let spawned = std::thread::Builder::new().name("pulsar-gst-watch".into()).spawn(move || {
		const KEY_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
		let mut next_key = std::time::Instant::now() + KEY_EVERY;
		while !stopped.load(Ordering::Relaxed) {
			match &bus {
				Some(bus) => {
					if let Some(msg) = bus.timed_pop_filtered(
						gst::ClockTime::from_mseconds(100),
						&[gst::MessageType::Error, gst::MessageType::Eos],
					) {
						match msg.view() {
							gst::MessageView::Error(e) => {
								tracing::error!(
									src = ?e.src().map(|s| s.path_string()),
									"gst pipeline error: {} ({:?})",
									e.error(),
									e.debug()
								);
								dead.store(true, Ordering::Relaxed);
							}
							gst::MessageView::Eos(_) => {
								tracing::warn!("gst pipeline reached EOS");
								dead.store(true, Ordering::Relaxed);
							}
							_ => {}
						}
					}
				}
				None => std::thread::sleep(std::time::Duration::from_millis(100)),
			}
			let now = std::time::Instant::now();
			if now >= next_key {
				next_key = now + KEY_EVERY;
				if short_gop.load(Ordering::Relaxed) && !dead.load(Ordering::Relaxed) {
					if let Some(enc) = &venc {
						let _ = force_key_unit(enc);
					}
				}
			}
		}
	});
	if let Err(e) = spawned {
		tracing::warn!("gst watch thread could not start ({e}) — pipeline errors go unnoticed");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A videotestsrc → x264enc pipeline through the portal-less path: frames are counted, the
	/// meter reports, a live bitrate change + keyframe request are accepted, stop is clean.
	/// Needs GStreamer with x264enc (gst-plugins-ugly); skips when it isn't there.
	#[test]
	fn start_pipeline_runs_with_live_controls() {
		if gst::init().is_err() || gst::ElementFactory::find("x264enc").is_none() {
			eprintln!("skipped: no GStreamer / x264enc");
			return;
		}
		let frag = crate::pipeline::gst::encoder_fragment(
			crate::pipeline::gst::GstEncoder::X264,
			crate::pipeline::VCodec::H264,
			2000,
			30,
		)
		.unwrap();
		let desc = format!(
			"videotestsrc is-live=true ! video/x-raw,width=320,height=180,framerate=30/1 \
			 ! videoconvert ! video/x-raw,format=I420 ! {frag} ! fakesink sync=false"
		);
		let reports = Arc::new(AtomicU64::new(0));
		let r = reports.clone();
		let cap = start_pipeline(&desc, Some(Box::new(move |_| {
			r.fetch_add(1, Ordering::Relaxed);
		})))
		.expect("pipeline starts");
		assert!(cap.is_in_process());
		assert!(cap.set_bitrate(1500));
		assert!(cap.request_keyframe());
		cap.set_short_gop(true);
		std::thread::sleep(std::time::Duration::from_millis(2500));
		assert!(cap.is_alive());
		assert!(cap.frames_encoded() > 30, "frames: {}", cap.frames_encoded());
		assert!(reports.load(Ordering::Relaxed) >= 1);
		drop(cap);
	}

	#[test]
	fn start_pipeline_refuses_a_missing_element() {
		if gst::init().is_err() {
			return;
		}
		let err = start_pipeline("videotestsrc ! pulsar-no-such-element ! fakesink", None)
			.err()
			.expect("missing element");
		assert!(err.to_string().contains("pulsar-no-such-element"), "{err}");
	}
}
