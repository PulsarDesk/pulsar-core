//! Raw-frame Wayland capture: the portal's PipeWire stream converted to NV12 in GStreamer and
//! handed, frame by frame, to an encoder the APP supplies ([`FrameEncoder`]) instead of a gst
//! encoder element.
//!
//! Why: the gst encoders (`nvh264enc`, `vah264enc`, `x264enc`, …) cannot invalidate a reference
//! frame. The desktop app's NVENC SDK encoder can (reference-frame invalidation heals a
//! client-reported loss without a keyframe), so on an NVIDIA host the app plugs that in here
//! and packetizes the stream itself. Pipeline:
//!
//! ```text
//! pipewiresrc ! queue leaky ! videoconvert ! video/x-raw,format=NV12 ! fakesink (handoff)
//! ```
//!
//! The encoder is built on the first frame (its size is the cast's, known only then) by the
//! app's [`FrameEncoderFactory`], and rebuilt when the size changes (a resolution change of the
//! streamed monitor). `start` waits for that first build: when it fails the caller falls back to
//! the gst-encoder pipeline on the same portal session, so the client never sees the attempt.
//!
//! Untested on real hardware (no Wayland + NVIDIA test host yet).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

/// One NV12 picture in system memory (GStreamer's mapped buffer, valid for the call only).
pub struct RawFrame<'a> {
	pub width: u32,
	pub height: u32,
	/// Luma plane and its row stride in bytes.
	pub y: &'a [u8],
	pub y_stride: usize,
	/// Interleaved CbCr plane (half height) and its row stride in bytes.
	pub uv: &'a [u8],
	pub uv_stride: usize,
}

/// An encoder that takes raw NV12 frames and sends the stream itself (RTP to the session's
/// video port). Called from GStreamer's streaming thread for the frames and from the session
/// handlers for the controls — never at the same time (one mutex around it).
pub trait FrameEncoder: Send {
	/// Encode and send one frame. An `Err` drops that frame (logged, rate-limited).
	fn encode(&mut self, frame: &RawFrame<'_>) -> Result<(), String>;
	/// Live target bitrate (kbit/s); `false` when refused.
	fn set_bitrate(&mut self, kbps: u32) -> bool;
	/// The next frame is a keyframe (with parameter sets).
	fn request_keyframe(&mut self);
	/// Reference-frame invalidation for the lost RTP packets `first..=last`; `false` when the
	/// encoder declined (out of reach, no RFI) — the client then asks for a keyframe itself.
	fn request_rfi(&mut self, first: u16, last: u16) -> bool;
}

/// Builds the encoder for a `width × height` stream (called on the streaming thread).
pub type FrameEncoderFactory = Box<dyn FnMut(u32, u32) -> Result<Box<dyn FrameEncoder>, String> + Send>;

/// The `gst-launch` description of the raw-frame pipeline (see the module doc).
pub(crate) fn pipeline_desc(fd: i32, node_id: u32) -> String {
	format!(
		"pipewiresrc fd={fd} path={node_id} do-timestamp=true keepalive-time=1000 \
		 ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
		 ! videoconvert ! video/x-raw,format=NV12 \
		 ! fakesink name={SINK_NAME} sync=false signal-handoffs=true"
	)
}

const SINK_NAME: &str = "psink";

/// The encoder slot the streaming thread and the controls share.
pub(crate) struct Slot {
	enc: Mutex<SlotState>,
	/// The first encoder was built (start may return).
	ready: AtomicBool,
	/// A build failed (start falls back; mid-stream it marks the capture dead).
	failed: Mutex<Option<String>>,
	dead: Arc<AtomicBool>,
}

struct SlotState {
	factory: FrameEncoderFactory,
	enc: Option<Box<dyn FrameEncoder>>,
	size: (u32, u32),
	last_err_log: Option<Instant>,
}

impl Slot {
	/// Run `f` on the live encoder (no-op `None` before the first frame).
	pub(crate) fn with<R>(&self, f: impl FnOnce(&mut (dyn FrameEncoder + 'static)) -> R) -> Option<R> {
		let mut st = self.enc.lock().unwrap_or_else(|e| e.into_inner());
		st.enc.as_mut().map(|e| f(e.as_mut()))
	}

	fn on_frame(&self, frame: &RawFrame<'_>) {
		if self.dead.load(Ordering::Relaxed) {
			return;
		}
		let mut st = self.enc.lock().unwrap_or_else(|e| e.into_inner());
		let size = (frame.width, frame.height);
		if st.enc.is_none() || st.size != size {
			// Drop the old one first (frees its NVENC session before a new one opens).
			st.enc = None;
			match (st.factory)(size.0, size.1) {
				Ok(e) => {
					tracing::info!(w = size.0, h = size.1, "wayland raw-frame encoder built");
					st.enc = Some(e);
					st.size = size;
					self.ready.store(true, Ordering::SeqCst);
				}
				Err(e) => {
					tracing::warn!(w = size.0, h = size.1, "wayland raw-frame encoder build failed: {e}");
					*self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
					if self.ready.load(Ordering::SeqCst) {
						// Mid-stream (a resize): nothing encodes any more.
						self.dead.store(true, Ordering::Relaxed);
					}
					return;
				}
			}
		}
		let r = st.enc.as_deref_mut().map(|e| e.encode(frame));
		if let Some(Err(e)) = r {
			let due = st.last_err_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(5));
			if due {
				tracing::warn!("wayland raw-frame encode failed: {e}");
				st.last_err_log = Some(Instant::now());
			}
		}
	}
}

/// The NV12 planes of a mapped frame, if the layout is the packed two-plane one.
fn planes<'a>(f: &'a gst_video::VideoFrameRef<&gst::BufferRef>) -> Option<RawFrame<'a>> {
	if f.format() != gst_video::VideoFormat::Nv12 || f.n_planes() < 2 {
		return None;
	}
	let strides = f.plane_stride();
	let (sy, suv) = (*strides.first()?, *strides.get(1)?);
	if sy <= 0 || suv <= 0 {
		return None;
	}
	Some(RawFrame {
		width: f.width(),
		height: f.height(),
		y: f.plane_data(0).ok()?,
		y_stride: sy as usize,
		uv: f.plane_data(1).ok()?,
		uv_stride: suv as usize,
	})
}

/// Build the raw-frame pipeline on the portal's node, set it PLAYING and wait (bounded) until the
/// first frame built the encoder. Err (pipeline torn down) = the caller uses the gst encoders.
pub(crate) async fn run(
	fd: i32,
	node_id: u32,
	factory: FrameEncoderFactory,
	dead: Arc<AtomicBool>,
) -> anyhow::Result<(gst::Pipeline, Arc<Slot>)> {
	let desc = pipeline_desc(fd, node_id);
	let pipeline = gst::parse::launch(&desc)
		.map_err(|e| anyhow::anyhow!("raw-frame pipeline parse failed: {e}"))?
		.downcast::<gst::Pipeline>()
		.map_err(|_| anyhow::anyhow!("raw-frame description did not build a pipeline"))?;
	let sink = pipeline
		.by_name(SINK_NAME)
		.ok_or_else(|| anyhow::anyhow!("raw-frame pipeline has no sink"))?;
	let slot = Arc::new(Slot {
		enc: Mutex::new(SlotState { factory, enc: None, size: (0, 0), last_err_log: None }),
		ready: AtomicBool::new(false),
		failed: Mutex::new(None),
		dead,
	});
	let s = slot.clone();
	// The caps of the sink pad, parsed once per change.
	let info: Mutex<Option<(gst::Caps, gst_video::VideoInfo)>> = Mutex::new(None);
	sink.connect("handoff", false, move |vals| {
		let buf = vals.get(1).and_then(|v| v.get::<gst::Buffer>().ok())?;
		let pad = vals.get(2).and_then(|v| v.get::<gst::Pad>().ok())?;
		let caps = pad.current_caps()?;
		let mut cached = info.lock().unwrap_or_else(|e| e.into_inner());
		if cached.as_ref().is_none_or(|(c, _)| *c != caps) {
			let vi = gst_video::VideoInfo::from_caps(&caps).ok()?;
			*cached = Some((caps, vi));
		}
		let (_, vi) = cached.as_ref()?;
		let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf.as_ref(), vi).ok()?;
		if let Some(raw) = planes(&frame) {
			s.on_frame(&raw);
		}
		None
	});
	pipeline
		.set_state(gst::State::Playing)
		.map_err(|e| anyhow::anyhow!("raw-frame pipeline could not start: {e}"))?;
	// Wait for the first frame's encoder (PipeWire links the node, the first buffer arrives).
	let t0 = Instant::now();
	let err = loop {
		if slot.ready.load(Ordering::SeqCst) {
			return Ok((pipeline, slot));
		}
		if let Some(e) = slot.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
			break e;
		}
		if let Some(msg) = pipeline.bus().and_then(|b| b.pop_filtered(&[gst::MessageType::Error])) {
			if let gst::MessageView::Error(e) = msg.view() {
				break format!("raw-frame pipeline error: {}", e.error());
			}
		}
		if t0.elapsed() > Duration::from_secs(4) {
			break "no frame from PipeWire within 4 s".to_string();
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	};
	let _ = pipeline.set_state(gst::State::Null);
	Err(anyhow::anyhow!(err))
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Counting {
		frames: Arc<Mutex<Vec<(u32, u32, usize)>>>,
	}

	impl FrameEncoder for Counting {
		fn encode(&mut self, f: &RawFrame<'_>) -> Result<(), String> {
			self.frames.lock().unwrap().push((f.width, f.height, f.y_stride));
			Ok(())
		}
		fn set_bitrate(&mut self, _: u32) -> bool {
			true
		}
		fn request_keyframe(&mut self) {}
		fn request_rfi(&mut self, _: u16, _: u16) -> bool {
			false
		}
	}

	#[test]
	fn pipeline_desc_ends_in_a_handoff_sink() {
		let d = pipeline_desc(7, 42);
		assert!(d.starts_with("pipewiresrc fd=7 path=42 "));
		assert!(d.contains("video/x-raw,format=NV12"));
		assert!(d.ends_with("fakesink name=psink sync=false signal-handoffs=true"));
	}

	/// The same sink + handoff plumbing with `videotestsrc` in place of PipeWire: the factory
	/// is called once with the stream size and every frame reaches the encoder.
	#[test]
	fn handoff_frames_reach_the_encoder() {
		gst::init().unwrap();
		let frames = Arc::new(Mutex::new(Vec::new()));
		let built = Arc::new(Mutex::new(Vec::new()));
		let (f2, b2) = (frames.clone(), built.clone());
		let factory: FrameEncoderFactory = Box::new(move |w, h| {
			b2.lock().unwrap().push((w, h));
			Ok(Box::new(Counting { frames: f2.clone() }) as Box<dyn FrameEncoder>)
		});
		let slot = Arc::new(Slot {
			enc: Mutex::new(SlotState { factory, enc: None, size: (0, 0), last_err_log: None }),
			ready: AtomicBool::new(false),
			failed: Mutex::new(None),
			dead: Arc::new(AtomicBool::new(false)),
		});
		let desc = format!(
			"videotestsrc num-buffers=5 ! video/x-raw,width=322,height=180 ! videoconvert \
			 ! video/x-raw,format=NV12 ! fakesink name={SINK_NAME} sync=false signal-handoffs=true"
		);
		let pipeline = gst::parse::launch(&desc).unwrap().downcast::<gst::Pipeline>().unwrap();
		let sink = pipeline.by_name(SINK_NAME).unwrap();
		let s = slot.clone();
		sink.connect("handoff", false, move |vals| {
			let buf = vals[1].get::<gst::Buffer>().ok()?;
			let pad = vals[2].get::<gst::Pad>().ok()?;
			let caps = pad.current_caps()?;
			let vi = gst_video::VideoInfo::from_caps(&caps).ok()?;
			let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf.as_ref(), &vi).ok()?;
			s.on_frame(&planes(&frame)?);
			None
		});
		pipeline.set_state(gst::State::Playing).unwrap();
		let bus = pipeline.bus().unwrap();
		let _ = bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos, gst::MessageType::Error]);
		let _ = pipeline.set_state(gst::State::Null);
		assert_eq!(*built.lock().unwrap(), vec![(322, 180)]);
		let got = frames.lock().unwrap();
		assert_eq!(got.len(), 5);
		assert!(got.iter().all(|&(w, h, stride)| w == 322 && h == 180 && stride >= 322));
		assert!(slot.ready.load(Ordering::SeqCst));
		assert_eq!(slot.with(|e| e.request_rfi(1, 2)), Some(false));
	}
}
