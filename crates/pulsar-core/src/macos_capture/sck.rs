//! ScreenCaptureKit capture: one `SCStream` per call of [`ScStream::start`], on one display,
//! delivering NV12 frames (cursor composited by the window server) and/or the system audio mix
//! without Pulsar's own process.
//!
//! Everything is resolved at run time (see the `ffi` module): an older macOS gets
//! [`ScError::Unavailable`] and the caller keeps its ffmpeg path.
//!
//! Threading: SCK calls the output object on a private serial dispatch queue (user-interactive
//! QoS). The callbacks given to [`ScStream::start`] run there and must return quickly — the
//! video one only hands a retained pixel buffer over to the encoder thread.
//!
//! Untested on macOS hardware.

use std::ffi::{c_void, CStr};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass};

use super::ffi;
use super::pcm;

/// `SCStreamErrorUserDeclined`: the Screen Recording permission was not given.
const SC_ERROR_USER_DECLINED: isize = -3801;
/// `SCStreamOutputTypeScreen` / `SCStreamOutputTypeAudio`.
const OUTPUT_SCREEN: isize = 0;
const OUTPUT_AUDIO: isize = 1;
/// `SCFrameStatusComplete`: a frame with new content (Idle/Blank frames carry no image).
const FRAME_STATUS_COMPLETE: i64 = 0;
/// How long the asynchronous SCK calls (content query, start, stop) may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a capture could not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScError {
	/// ScreenCaptureKit (macOS 12.3+), or SCK audio (macOS 13+), is not on this Mac.
	Unavailable(&'static str),
	/// The Screen Recording permission is missing (System Settings → Privacy & Security →
	/// Screen & System Audio Recording). SCK audio needs it too.
	PermissionDenied,
	/// No active display with this `CGDirectDisplayID`.
	NoDisplay(u32),
	/// Anything else SCK reported.
	Failed(String),
}

impl std::fmt::Display for ScError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unavailable(what) => write!(f, "ScreenCaptureKit unavailable: {what}"),
			Self::PermissionDenied => write!(f, "screen recording permission denied"),
			Self::NoDisplay(id) => write!(f, "no active display {id}"),
			Self::Failed(e) => write!(f, "ScreenCaptureKit: {e}"),
		}
	}
}

impl std::error::Error for ScError {}

/// One active display, in the host's index order (the main display first).
#[derive(Clone, Debug, PartialEq)]
pub struct MacDisplay {
	pub id: u32,
	pub name: String,
	/// Native mode size in pixels (a Retina panel's backing size).
	pub width: u32,
	pub height: u32,
	pub primary: bool,
	/// Place in the global display space, in points (CGEvent / CGDisplayBounds coordinates).
	pub left: i32,
	pub top: i32,
	pub width_pt: u32,
	pub height_pt: u32,
	/// Current refresh rate (0 = unknown; some built-in panels report 0).
	pub refresh_hz: u32,
}

/// The active displays, main display first, then CoreGraphics order. This order is the
/// display index the host advertises and the native capture takes.
pub fn displays() -> Vec<MacDisplay> {
	let mut ids = [0u32; 32];
	let mut count = 0u32;
	// SAFETY: plain CoreGraphics query into a local array.
	if unsafe { ffi::CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
		return Vec::new();
	}
	// SAFETY: no arguments.
	let main = unsafe { ffi::CGMainDisplayID() };
	let mut list: Vec<MacDisplay> = ids[..count as usize]
		.iter()
		.map(|&id| {
			// SAFETY: `id` came from the active display list; the mode is released below.
			unsafe {
				let b = ffi::CGDisplayBounds(id);
				let mode = ffi::CGDisplayCopyDisplayMode(id);
				let (w, h, hz) = if mode.is_null() {
					(b.size.width as u32, b.size.height as u32, 0)
				} else {
					let r = (
						ffi::CGDisplayModeGetPixelWidth(mode) as u32,
						ffi::CGDisplayModeGetPixelHeight(mode) as u32,
						ffi::CGDisplayModeGetRefreshRate(mode).round() as u32,
					);
					ffi::CGDisplayModeRelease(mode);
					r
				};
				let builtin = ffi::CGDisplayIsBuiltin(id) != 0;
				MacDisplay {
					id,
					name: if builtin { "Built-in Display".to_string() } else { format!("Display {id}") },
					width: w,
					height: h,
					primary: id == main,
					left: b.origin.x as i32,
					top: b.origin.y as i32,
					width_pt: b.size.width as u32,
					height_pt: b.size.height as u32,
					refresh_hz: hz,
				}
			}
		})
		.collect();
	list.sort_by_key(|d| !d.primary);
	list
}

/// Whether this Mac has ScreenCaptureKit (macOS 12.3+).
pub fn sck_available() -> bool {
	ffi::load_screencapturekit() && AnyClass::get(c"SCStream").is_some()
}

/// Whether the app may record the screen (the Screen Recording TCC grant). With `prompt` and no
/// grant yet, macOS shows its permission dialog once (later calls only answer); the grant
/// takes effect after the app is restarted. `true` on a macOS older than 10.15 (no such
/// permission there).
pub fn screen_capture_access(prompt: bool) -> bool {
	let Some(preflight) = ffi::screen_capture_access_fn(false) else {
		return true;
	};
	if preflight() {
		return true;
	}
	if prompt {
		if let Some(request) = ffi::screen_capture_access_fn(true) {
			return request();
		}
	}
	false
}

/// The video side of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoSpec {
	/// Output size in pixels (SCK scales the display into it, keeping the aspect ratio).
	pub width: u32,
	pub height: u32,
	/// Highest frame rate delivered (`minimumFrameInterval` = 1/fps). SCK only sends a frame
	/// when the screen changed.
	pub fps: u32,
	/// Composite the pointer into the frames.
	pub cursor: bool,
}

/// The audio side of a stream (macOS 13+). Always leaves this process's own audio out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpec {
	pub sample_rate: u32,
	pub channels: u32,
}

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSpec {
	/// `CGDirectDisplayID`; 0 = the main display.
	pub display_id: u32,
	pub video: Option<VideoSpec>,
	pub audio: Option<AudioSpec>,
}

/// A captured frame: a retained `CVPixelBuffer` (NV12, video range, IOSurface-backed).
pub struct VideoFrame {
	pb: ffi::CVPixelBufferRef,
	/// When SCK handed it to us.
	pub arrived: Instant,
}

// SAFETY: a CVPixelBuffer is a reference-counted CF object; we own one reference and only read.
unsafe impl Send for VideoFrame {}
unsafe impl Sync for VideoFrame {}

impl VideoFrame {
	/// The `CVPixelBufferRef` (borrowed; valid while `self` lives).
	pub fn pixel_buffer(&self) -> *mut c_void {
		self.pb
	}

	pub fn width(&self) -> u32 {
		// SAFETY: `pb` is a live pixel buffer.
		unsafe { ffi::CVPixelBufferGetWidth(self.pb) as u32 }
	}

	pub fn height(&self) -> u32 {
		// SAFETY: `pb` is a live pixel buffer.
		unsafe { ffi::CVPixelBufferGetHeight(self.pb) as u32 }
	}
}

impl Clone for VideoFrame {
	fn clone(&self) -> Self {
		// SAFETY: one more reference to a live CF object, released by the clone's Drop.
		unsafe { ffi::CFRetain(self.pb as ffi::CFTypeRef) };
		Self { pb: self.pb, arrived: self.arrived }
	}
}

impl Drop for VideoFrame {
	fn drop(&mut self) {
		// SAFETY: releases the reference this value owns.
		unsafe { ffi::CFRelease(self.pb as ffi::CFTypeRef) };
	}
}

/// Called with every frame that has new content.
pub type VideoCallback = Box<dyn Fn(VideoFrame) + Send + Sync>;
/// Called with interleaved stereo f32 PCM at the configured sample rate.
pub type AudioCallback = Box<dyn Fn(&[f32]) + Send + Sync>;
/// Called once when SCK stops the stream on its own (display gone, permission revoked, …).
pub type StopCallback = Box<dyn Fn(String) + Send + Sync>;

/// The output object's state.
pub struct OutputIvars {
	video: Option<VideoCallback>,
	audio: Option<AudioCallback>,
	on_stop: Option<StopCallback>,
	/// `SCStreamFrameInfoStatus` (the attachment key), resolved once.
	status_key: ffi::CFStringRef,
	/// Scratch buffers of the audio path (only the dispatch queue touches them).
	scratch: Mutex<(Vec<f32>, Vec<f32>)>,
}

// SAFETY: `status_key` is an immutable CFString constant; the rest is Send + Sync.
unsafe impl Send for OutputIvars {}
unsafe impl Sync for OutputIvars {}

define_class!(
	// SAFETY: NSObject has no subclassing requirements; the class implements no Drop.
	#[unsafe(super(NSObject))]
	#[name = "PulsarScStreamOutput"]
	#[ivars = OutputIvars]
	pub struct ScOutput;

	impl ScOutput {
		/// `-[SCStreamOutput stream:didOutputSampleBuffer:ofType:]`.
		#[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
		fn did_output(&self, _stream: *mut AnyObject, sample: *mut c_void, kind: isize) {
			let iv = self.ivars();
			let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				// SAFETY: SCK passes a valid CMSampleBuffer for the duration of the call.
				unsafe {
					match kind {
						OUTPUT_SCREEN => on_video_sample(iv, sample),
						OUTPUT_AUDIO => on_audio_sample(iv, sample),
						_ => {}
					}
				}
			}));
		}

		/// `-[SCStreamDelegate stream:didStopWithError:]`.
		#[unsafe(method(stream:didStopWithError:))]
		fn did_stop(&self, _stream: *mut AnyObject, error: *mut AnyObject) {
			let text = ns_error_text(error);
			tracing::warn!(error = %text, "ScreenCaptureKit stream stopped");
			if let Some(cb) = &self.ivars().on_stop {
				let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(text)));
			}
		}
	}
);

impl ScOutput {
	fn new(ivars: OutputIvars) -> Retained<Self> {
		let this = Self::alloc().set_ivars(ivars);
		// SAFETY: NSObject's designated initializer.
		unsafe { msg_send![super(this), init] }
	}
}

unsafe fn on_video_sample(iv: &OutputIvars, sample: *mut c_void) {
	let Some(cb) = &iv.video else { return };
	if sample.is_null() || ffi::CMSampleBufferIsValid(sample) == 0 {
		return;
	}
	// Only "complete" frames carry new content; idle/blank ones have no image buffer.
	let atts = ffi::CMSampleBufferGetSampleAttachmentsArray(sample, 0);
	if !atts.is_null() && ffi::CFArrayGetCount(atts) > 0 && !iv.status_key.is_null() {
		let dict = ffi::CFArrayGetValueAtIndex(atts, 0);
		let status = ffi::cf_number_i64(ffi::CFDictionaryGetValue(dict, iv.status_key));
		if status.is_some_and(|s| s != FRAME_STATUS_COMPLETE) {
			return;
		}
	}
	let pb = ffi::CMSampleBufferGetImageBuffer(sample);
	if pb.is_null() {
		return;
	}
	ffi::CFRetain(pb as ffi::CFTypeRef);
	cb(VideoFrame { pb, arrived: Instant::now() });
}

unsafe fn on_audio_sample(iv: &OutputIvars, sample: *mut c_void) {
	let Some(cb) = &iv.audio else { return };
	if sample.is_null() || ffi::CMSampleBufferIsValid(sample) == 0 || ffi::CMSampleBufferDataIsReady(sample) == 0 {
		return;
	}
	let desc = ffi::CMSampleBufferGetFormatDescription(sample);
	if desc.is_null() {
		return;
	}
	let asbd = ffi::CMAudioFormatDescriptionGetStreamBasicDescription(desc);
	if asbd.is_null() {
		return;
	}
	let asbd = *asbd;
	if asbd.mFormatID != ffi::kAudioFormatLinearPCM || asbd.mChannelsPerFrame == 0 {
		return;
	}
	let frames = ffi::CMSampleBufferGetNumSamples(sample).max(0) as usize;
	if frames == 0 {
		return;
	}
	let mut abl = ffi::AudioBufferList16::default();
	let mut block: ffi::CMBlockBufferRef = std::ptr::null_mut();
	let st = ffi::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
		sample,
		std::ptr::null_mut(),
		&mut abl,
		std::mem::size_of::<ffi::AudioBufferList16>(),
		std::ptr::null(),
		std::ptr::null(),
		ffi::kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
		&mut block,
	);
	if st != 0 {
		return;
	}
	let channels = asbd.mChannelsPerFrame as usize;
	let planar = asbd.mFormatFlags & ffi::kAudioFormatFlagIsNonInterleaved != 0;
	let float = asbd.mFormatFlags & ffi::kAudioFormatFlagIsFloat != 0 && asbd.mBitsPerChannel == 32;
	let int16 = asbd.mFormatFlags & ffi::kAudioFormatFlagIsSignedInteger != 0 && asbd.mBitsPerChannel == 16;
	let n = (abl.mNumberBuffers as usize).min(abl.mBuffers.len());
	let bufs = &abl.mBuffers[..n];
	if let Ok(mut scratch) = iv.scratch.lock() {
		let (inter, stereo) = &mut *scratch;
		let mut ok = true;
		if planar && float {
			let planes: Vec<&[f32]> = bufs
				.iter()
				.filter(|b| !b.mData.is_null())
				.map(|b| std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4))
				.collect();
			pcm::interleave_planar(&planes, frames, inter);
		} else if !planar && float && n >= 1 && !bufs[0].mData.is_null() {
			let s = std::slice::from_raw_parts(bufs[0].mData as *const f32, bufs[0].mDataByteSize as usize / 4);
			inter.clear();
			inter.extend_from_slice(&s[..s.len().min(frames * channels)]);
		} else if !planar && int16 && n >= 1 && !bufs[0].mData.is_null() {
			let s = std::slice::from_raw_parts(bufs[0].mData as *const i16, bufs[0].mDataByteSize as usize / 2);
			pcm::i16_to_f32(&s[..s.len().min(frames * channels)], inter);
		} else {
			ok = false;
		}
		if ok {
			// Planar input was interleaved with one entry per plane present.
			let width = if planar { bufs.iter().filter(|b| !b.mData.is_null()).count() } else { channels };
			pcm::to_stereo(inter, width, stereo);
			if !stereo.is_empty() {
				cb(stereo);
			}
		}
	}
	if !block.is_null() {
		ffi::CFRelease(block as ffi::CFTypeRef);
	}
}

/// `-[NSError localizedDescription]` (+ code), or a fixed text for nil.
fn ns_error_text(err: *mut AnyObject) -> String {
	if err.is_null() {
		return "no error".to_string();
	}
	// SAFETY: `err` is an NSError.
	unsafe {
		let code: isize = msg_send![err, code];
		let desc: Option<Retained<AnyObject>> = msg_send![err, localizedDescription];
		let text = desc.map(|d| ns_string(&d)).unwrap_or_default();
		format!("{text} (code {code})")
	}
}

fn ns_error_code(err: *mut AnyObject) -> isize {
	if err.is_null() {
		return 0;
	}
	// SAFETY: `err` is an NSError.
	unsafe { msg_send![err, code] }
}

/// The text of an NSString, read through its toll-free bridged CFString (no `UTF8String`
/// message: its `r*` return encoding is not one objc2's debug check takes for a C string).
unsafe fn ns_string(s: &AnyObject) -> String {
	ffi::cf_string_to_string((s as *const AnyObject).cast())
}

fn class(name: &CStr) -> Result<&'static AnyClass, ScError> {
	AnyClass::get(name).ok_or(ScError::Unavailable("class missing"))
}

/// A retained ObjC object moved across the completion-handler thread.
struct SendObj(Option<Retained<AnyObject>>);
// SAFETY: SCShareableContent is an immutable snapshot; it is only read after the hand-over.
unsafe impl Send for SendObj {}

/// `+[SCShareableContent getShareableContentWithCompletionHandler:]`, waited for.
fn shareable_content() -> Result<Retained<AnyObject>, ScError> {
	let cls = class(c"SCShareableContent")?;
	let (tx, rx) = mpsc::channel::<Result<SendObj, (isize, String)>>();
	let block = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
		let r = if !error.is_null() {
			Err((ns_error_code(error), ns_error_text(error)))
		} else {
			// SAFETY: SCK passes a valid object (or nil) for the duration of the handler.
			Ok(SendObj(unsafe { Retained::retain(content) }))
		};
		let _ = tx.send(r);
	});
	// SAFETY: class method taking one completion block.
	let _: () = unsafe { msg_send![cls, getShareableContentWithCompletionHandler: &*block] };
	match rx.recv_timeout(CALL_TIMEOUT) {
		Ok(Ok(SendObj(Some(c)))) => Ok(c),
		Ok(Ok(SendObj(None))) => Err(ScError::Failed("no shareable content".into())),
		Ok(Err((code, _))) if code == SC_ERROR_USER_DECLINED => Err(ScError::PermissionDenied),
		Ok(Err((_, text))) => {
			if !screen_capture_access(false) {
				Err(ScError::PermissionDenied)
			} else {
				Err(ScError::Failed(text))
			}
		}
		Err(_) => Err(ScError::Failed("shareable content query timed out".into())),
	}
}

/// The `SCDisplay` with this id (0 = main) in `content`.
unsafe fn find_display(content: &AnyObject, id: u32) -> Result<Retained<AnyObject>, ScError> {
	let want = if id == 0 { ffi::CGMainDisplayID() } else { id };
	let list: Option<Retained<AnyObject>> = msg_send![content, displays];
	let Some(list) = list else { return Err(ScError::NoDisplay(want)) };
	let n: usize = msg_send![&*list, count];
	for i in 0..n {
		let d: Option<Retained<AnyObject>> = msg_send![&*list, objectAtIndex: i];
		if let Some(d) = d {
			let did: u32 = msg_send![&*d, displayID];
			if did == want {
				return Ok(d);
			}
		}
	}
	Err(ScError::NoDisplay(want))
}

unsafe fn responds(obj: &AnyObject, sel: Sel) -> bool {
	let r: Bool = msg_send![obj, respondsToSelector: sel];
	r.as_bool()
}

/// Wait for an SCK completion handler of the `(NSError *)` shape.
fn call_with_completion(f: impl FnOnce(&block2::Block<dyn Fn(*mut AnyObject)>)) -> Result<(), ScError> {
	let (tx, rx) = mpsc::channel::<Result<(), (isize, String)>>();
	let block = RcBlock::new(move |error: *mut AnyObject| {
		let _ = tx.send(if error.is_null() { Ok(()) } else { Err((ns_error_code(error), ns_error_text(error))) });
	});
	f(&block);
	match rx.recv_timeout(CALL_TIMEOUT) {
		Ok(Ok(())) => Ok(()),
		Ok(Err((code, _))) if code == SC_ERROR_USER_DECLINED => Err(ScError::PermissionDenied),
		Ok(Err((_, text))) => Err(ScError::Failed(text)),
		Err(_) => Err(ScError::Failed("timed out".into())),
	}
}

/// A running `SCStream`. Stops on drop.
pub struct ScStream {
	stream: Retained<AnyObject>,
	_output: Retained<ScOutput>,
	queue: *mut c_void,
	running: bool,
}

// SAFETY: SCStream may be started/stopped from any thread; the queue is a libdispatch object.
unsafe impl Send for ScStream {}

impl ScStream {
	/// Build and start a stream. Waits (≤ 5 s per step) for SCK's asynchronous answers, so call
	/// it off the main thread.
	pub fn start(
		spec: StreamSpec,
		on_video: Option<VideoCallback>,
		on_audio: Option<AudioCallback>,
		on_stop: Option<StopCallback>,
	) -> Result<Self, ScError> {
		if !sck_available() {
			return Err(ScError::Unavailable("macOS 12.3 or newer is needed"));
		}
		if !screen_capture_access(false) {
			return Err(ScError::PermissionDenied);
		}
		let content = shareable_content()?;
		// SAFETY: every message below is sent to the SCK object type that declares it; the
		// argument types match the declared encodings (checked by objc2 in debug builds).
		unsafe {
			let display = find_display(&content, spec.display_id)?;
			let empty: Retained<AnyObject> = msg_send![class(c"NSArray")?, array];
			let filter_alloc: Allocated<AnyObject> = msg_send![class(c"SCContentFilter")?, alloc];
			let filter: Option<Retained<AnyObject>> =
				msg_send![filter_alloc, initWithDisplay: &*display, excludingWindows: &*empty];
			let filter = filter.ok_or_else(|| ScError::Failed("content filter".into()))?;

			let cfg: Retained<AnyObject> = msg_send![class(c"SCStreamConfiguration")?, new];
			let (w, h, fps, cursor) = match spec.video {
				Some(v) => (v.width.max(2), v.height.max(2), v.fps.max(1), v.cursor),
				// Audio only: SCK still renders the display; keep that as small as it goes.
				None => (16, 16, 1, false),
			};
			let _: () = msg_send![&*cfg, setWidth: w as usize];
			let _: () = msg_send![&*cfg, setHeight: h as usize];
			let _: () = msg_send![&*cfg, setMinimumFrameInterval: ffi::CMTime::new(1, fps as i32)];
			let _: () = msg_send![&*cfg, setPixelFormat: ffi::PIXEL_FORMAT_NV12_VIDEO];
			let _: () = msg_send![&*cfg, setShowsCursor: Bool::new(cursor)];
			// A few frames of slack so the encoder thread holding one never stalls SCK.
			let _: () = msg_send![&*cfg, setQueueDepth: 5isize];
			if let Some(a) = spec.audio {
				if !responds(&cfg, sel!(setCapturesAudio:)) || !responds(&cfg, sel!(setExcludesCurrentProcessAudio:)) {
					return Err(ScError::Unavailable("system audio capture needs macOS 13 or newer"));
				}
				let _: () = msg_send![&*cfg, setCapturesAudio: Bool::YES];
				let _: () = msg_send![&*cfg, setSampleRate: a.sample_rate as isize];
				let _: () = msg_send![&*cfg, setChannelCount: a.channels as isize];
				let _: () = msg_send![&*cfg, setExcludesCurrentProcessAudio: Bool::YES];
			}

			let output = ScOutput::new(OutputIvars {
				video: on_video,
				audio: if spec.audio.is_some() { on_audio } else { None },
				on_stop,
				status_key: ffi::cf_string_const(c"SCStreamFrameInfoStatus").unwrap_or(std::ptr::null()),
				scratch: Mutex::new((Vec::new(), Vec::new())),
			});
			// The object lives on the heap until `ScStream` drops it; SCK retains it too.
			let out_ptr: *const AnyObject = Retained::as_ptr(&output).cast::<AnyObject>();
			let out_obj: &AnyObject = &*out_ptr;

			let stream_alloc: Allocated<AnyObject> = msg_send![class(c"SCStream")?, alloc];
			let stream: Option<Retained<AnyObject>> =
				msg_send![stream_alloc, initWithFilter: &*filter, configuration: &*cfg, delegate: out_obj];
			let stream = stream.ok_or_else(|| ScError::Failed("stream init".into()))?;

			let attr = ffi::dispatch_queue_attr_make_with_qos_class(std::ptr::null(), ffi::QOS_CLASS_USER_INTERACTIVE, 0);
			let queue = ffi::dispatch_queue_create(c"dev.pulsar.sck".as_ptr(), attr);
			if queue.is_null() {
				return Err(ScError::Failed("dispatch queue".into()));
			}
			let mut me = Self { stream, _output: output, queue, running: false };
			// The screen output is always added: an audio-only stream without one makes SCK log
			// a "stream output NOT found" line for every (tiny) frame it renders anyway.
			let mut kinds = vec![OUTPUT_SCREEN];
			if spec.audio.is_some() {
				kinds.push(OUTPUT_AUDIO);
			}
			for kind in kinds {
				let mut err: *mut AnyObject = std::ptr::null_mut();
				let ok: Bool = msg_send![
					&*me.stream,
					addStreamOutput: out_obj,
					type: kind,
					sampleHandlerQueue: me.queue as *mut AnyObject,
					error: &mut err
				];
				if !ok.as_bool() {
					return Err(ScError::Failed(format!("add output {kind}: {}", ns_error_text(err))));
				}
			}
			let stream_ref: &AnyObject = &me.stream;
			call_with_completion(|b| {
				let _: () = msg_send![stream_ref, startCaptureWithCompletionHandler: b];
			})?;
			me.running = true;
			tracing::info!(display = spec.display_id, w, h, fps, audio = spec.audio.is_some(), "ScreenCaptureKit stream started");
			Ok(me)
		}
	}

	/// Stop the stream (waits ≤ 5 s for SCK).
	pub fn stop(mut self) {
		self.stop_inner();
	}

	fn stop_inner(&mut self) {
		if !self.running {
			return;
		}
		self.running = false;
		let stream_ref: &AnyObject = &self.stream;
		// SAFETY: -[SCStream stopCaptureWithCompletionHandler:].
		let r = call_with_completion(|b| unsafe {
			let _: () = msg_send![stream_ref, stopCaptureWithCompletionHandler: b];
		});
		if let Err(e) = r {
			tracing::debug!("ScreenCaptureKit stop: {e}");
		}
	}
}

impl Drop for ScStream {
	fn drop(&mut self) {
		self.stop_inner();
		if !self.queue.is_null() {
			// SAFETY: the queue was created by us; SCK keeps its own reference while it uses it.
			unsafe { ffi::dispatch_release(self.queue) };
			self.queue = std::ptr::null_mut();
		}
	}
}

/// Keep `NSObject` referenced for the class definition above.
#[allow(dead_code)]
type _Super = NSObject;
