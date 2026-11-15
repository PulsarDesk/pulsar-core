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
//! Plus an encode meter: pad probes on the encoder count the frames it puts out and time each
//! frame inside it ([`EncodeTimer`]) — the real encode time, where the `gst-launch -v` pace
//! meter could only time the gap between frames. The count is also what [`start_pipeline`]
//! waits for: a live `ximagesrc` pipeline never prerolls (its sinks complete PLAYING only once
//! the first buffer reaches them), so "reached PLAYING" and "encoded a frame" are the same
//! moment there. The start waits for the first ENCODED frame directly and tells a bus error
//! ([`StartError::Failed`]) from a pipeline that runs but encodes nothing
//! ([`StartError::NoFirstFrame`], the RK3588 RGA >4G BGRx case), so the caller picks the right
//! fallback at once instead of after a second watchdog.
//!
//! TRADE-OFF: the vendor stack (`mpph26xenc` + librga/libmpp) now runs inside the app process.
//! A crash or abort in it takes the whole host app down (UI, every session, audio), where the
//! `gst-launch` child only lost its own stream. `PULSAR_GST_INPROC=0` forces the child; the
//! desktop also keeps a crash marker around each start (see its `host/handlers.rs`).
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

use super::{force_key_unit, missing_elements, parse_pipeline, Backend, WaylandCapture};
use crate::pipeline::gst::EncodeTimer;

/// Called about once a second with the smoothed per-frame encode time (ms), from a GStreamer
/// streaming thread — keep it cheap and non-blocking.
pub type EncodeMsFn = Box<dyn Fn(f32) + Send + Sync + 'static>;

/// Why [`start_pipeline`] gave up — the caller picks its fallback by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
	/// The in-process GStreamer can't run this description at all: init failed, an element is
	/// missing (an AppImage's bundled GStreamer without the system's Rockchip plugin), it does
	/// not parse, or it has no `venc` encoder to drive and meter. Run it under `gst-launch`.
	Unavailable(String),
	/// It started, then the state change was refused or the bus reported an error / EOS
	/// before the first encoded frame.
	Failed(String),
	/// No error, but the encoder put out nothing within the deadline (the RK3588 RGA >4G
	/// BGRx-direct case: the blitter silently never delivers a frame).
	NoFirstFrame,
}

impl std::fmt::Display for StartError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			StartError::Unavailable(m) => write!(f, "in-process gst unavailable: {m}"),
			StartError::Failed(m) => write!(f, "in-process gst pipeline failed before its first frame: {m}"),
			StartError::NoFirstFrame => write!(f, "in-process gst pipeline encoded no frame in time"),
		}
	}
}

impl std::error::Error for StartError {}

/// Run a complete pipeline description in this process (no portal, no PipeWire fd) and return
/// the same handle a Wayland capture gives: [`WaylandCapture::set_bitrate`],
/// [`WaylandCapture::request_keyframe`], [`WaylandCapture::set_short_gop`],
/// [`WaylandCapture::is_alive`], [`WaylandCapture::frames_encoded`]; dropping it (or `stop()`)
/// tears the pipeline down.
///
/// Returns once the encoder has put out its FIRST frame (so `frames_encoded() >= 1` on
/// success), or fails with a [`StartError`]: a bus error / EOS at once, no frame at all after
/// `first_frame_within`. The description must name its encoder `venc` (the meter and the live
/// controls hang off it). Blocking (up to `first_frame_within`, plus the teardown on a
/// failure): call it off any async loop. No tokio runtime needed.
pub fn start_pipeline(
	desc: &str,
	on_encode_ms: Option<EncodeMsFn>,
	first_frame_within: std::time::Duration,
) -> Result<WaylandCapture, StartError> {
	gst::init().map_err(|e| StartError::Unavailable(format!("init failed: {e}")))?;
	let missing = missing_elements(desc);
	if !missing.is_empty() {
		return Err(StartError::Unavailable(format!("lacks elements: {}", missing.join(", "))));
	}
	let (pipeline, venc) = parse_pipeline(desc).map_err(|e| StartError::Unavailable(e.to_string()))?;
	let Some(enc) = venc.clone() else {
		// No encoder to drive or meter: nothing here a `gst-launch` child can't do.
		return Err(StartError::Unavailable(format!("no `{}` encoder element", super::ENCODER_NAME)));
	};
	let encoded = Arc::new(AtomicU64::new(0));
	// The meter goes on before PLAYING so the very first frame is counted.
	if !attach_meter(&enc, encoded.clone(), on_encode_ms) {
		return Err(StartError::Unavailable("encoder has no static sink/src pad to meter".into()));
	}
	let dead = Arc::new(AtomicBool::new(false));
	let short_gop = Arc::new(AtomicBool::new(false));
	let stopped = Arc::new(AtomicBool::new(false));
	let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
	// From here on the handle owns the pipeline: an early return drops it, which stops the
	// watch thread and sets the pipeline to NULL.
	let cap = WaylandCapture {
		backend: Backend::InProcess { pipeline: pipeline.clone(), venc: venc.clone() },
		session: None,
		_pw_fd: None,
		encoded,
		dead: dead.clone(),
		short_gop: short_gop.clone(),
		stopped: stopped.clone(),
		pointer_region: None,
		stream_rect: None,
	};
	spawn_watch_thread(&pipeline, venc, dead.clone(), short_gop, stopped, error.clone());
	// A live source answers NO_PREROLL / ASYNC here — accepted; only an outright refusal fails.
	if let Err(e) = pipeline.set_state(gst::State::Playing) {
		// Give the watch thread a moment to pick the element's own error off the bus.
		std::thread::sleep(std::time::Duration::from_millis(150));
		let why = error.lock().unwrap_or_else(|p| p.into_inner()).take();
		return Err(StartError::Failed(why.unwrap_or_else(|| e.to_string())));
	}
	let deadline = std::time::Instant::now() + first_frame_within;
	loop {
		if cap.frames_encoded() > 0 {
			break;
		}
		if dead.load(Ordering::Relaxed) {
			let why = error.lock().unwrap_or_else(|p| p.into_inner()).take();
			return Err(StartError::Failed(why.unwrap_or_else(|| "bus error / EOS".into())));
		}
		if std::time::Instant::now() >= deadline {
			return Err(StartError::NoFirstFrame);
		}
		std::thread::sleep(std::time::Duration::from_millis(10));
	}
	tracing::info!("in-process gst pipeline running (no portal), first frame encoded");
	Ok(cap)
}

/// Pad probes on the encoder element: raw frames in (sink) and encoded frames out (src), timed
/// by PTS. Every buffer leaving the src pad counts as an encoded frame.
fn attach_meter(enc: &gst::Element, encoded: Arc<AtomicU64>, on_ms: Option<EncodeMsFn>) -> bool {
	let (Some(sink), Some(src)) = (enc.static_pad("sink"), enc.static_pad("src")) else {
		tracing::warn!("gst encoder has no static sink/src pad — no encode meter");
		return false;
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
	true
}

/// One std thread per pipeline: drains the bus (error / EOS → `dead`) and, while `short_gop`
/// is set, forces a key unit every 500 ms. Exits within ~100 ms of `stopped`. The first bus
/// error's text goes to `error` (the start reports it).
fn spawn_watch_thread(
	pipeline: &gst::Pipeline,
	venc: Option<gst::Element>,
	dead: Arc<AtomicBool>,
	short_gop: Arc<AtomicBool>,
	stopped: Arc<AtomicBool>,
	error: Arc<Mutex<Option<String>>>,
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
								let mut first = error.lock().unwrap_or_else(|p| p.into_inner());
								if first.is_none() {
									*first = Some(format!("{}", e.error()));
								}
								drop(first);
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
		let cap = start_pipeline(
			&desc,
			Some(Box::new(move |_| {
				r.fetch_add(1, Ordering::Relaxed);
			})),
			std::time::Duration::from_secs(3),
		)
		.expect("pipeline starts");
		// The start returns on the first encoded frame.
		assert!(cap.frames_encoded() >= 1);
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
		let err = start_pipeline(
			"videotestsrc ! pulsar-no-such-element ! fakesink",
			None,
			std::time::Duration::from_secs(1),
		)
		.err()
		.expect("missing element");
		assert!(matches!(err, StartError::Unavailable(_)), "{err}");
		assert!(err.to_string().contains("pulsar-no-such-element"), "{err}");
	}

	/// A pipeline that runs without an error but never feeds its encoder (a closed valve, the
	/// RGA >4G stand-in) fails with `NoFirstFrame` after the deadline, not a generic error.
	#[test]
	fn start_pipeline_reports_no_first_frame() {
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
			 ! valve drop=true ! videoconvert ! video/x-raw,format=I420 ! {frag} ! fakesink sync=false"
		);
		let t0 = std::time::Instant::now();
		let err = start_pipeline(&desc, None, std::time::Duration::from_millis(600))
			.err()
			.expect("no frame");
		assert_eq!(err, StartError::NoFirstFrame);
		assert!(t0.elapsed() < std::time::Duration::from_secs(3));
	}

	/// No `venc` element: nothing to drive live, so the caller runs a child instead.
	#[test]
	fn start_pipeline_wants_a_venc_encoder() {
		if gst::init().is_err() {
			return;
		}
		let err = start_pipeline("videotestsrc ! fakesink", None, std::time::Duration::from_secs(1))
			.err()
			.expect("no venc");
		assert!(matches!(err, StartError::Unavailable(_)), "{err}");
	}
}
