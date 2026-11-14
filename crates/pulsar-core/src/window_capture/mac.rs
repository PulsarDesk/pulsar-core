//! macOS per-window capture through **ScreenCaptureKit** (macOS 12.3+).
//!
//! * Listing: `SCShareableContent` (on-screen windows, desktop windows excluded), filtered
//!   like the Windows picker: normal window layer (0), titled (or named after the owning
//!   app), larger than 1×1, not this process's own windows.
//! * Capture: an `SCStream` over `SCContentFilter(desktopIndependentWindow:)`, i.e. ONE
//!   window with its own content only (other windows on top of it never show, it keeps
//!   streaming while covered). Frames arrive as NV12 (`420v`) `CVPixelBuffer`s of exactly
//!   the configured size; they are packed tight and served on a loopback TCP socket that
//!   ffmpeg reads as `-f rawvideo -pix_fmt nv12` ([`WindowInput::RawNv12`]), so the existing
//!   ffmpeg/VideoToolbox encode chain and its fallbacks stay as they are.
//! * Pacing: ScreenCaptureKit only delivers a frame when the window changes, while rawvideo
//!   timestamps count frames. A writer thread therefore sends the LATEST frame at a steady
//!   `fps` (repeating it on a still window) — the same "re-encode the last surface" rule the
//!   Windows native capture follows.
//!
//! Needs the Screen Recording permission (TCC) like the screen capture; without it the
//! list is empty and a start fails with ScreenCaptureKit's own error.
//!
//! Untested on macOS hardware.
//!
//! [`WindowInput::RawNv12`]: crate::pipeline::WindowInput::RawNv12

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
	kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBufferGetBaseAddressOfPlane,
	CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress,
	CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
	SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
	SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::HostWindow;

/// How long a ScreenCaptureKit completion handler may take before we give up.
const SCK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the frame server waits for ffmpeg to connect.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

/// Moves an Objective-C object across threads. ScreenCaptureKit objects are documented
/// as usable from any queue; objc2 just doesn't mark the generated types `Send`.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

/// ScreenCaptureKit exists (macOS 12.3+). Checked at runtime: objc2 looks classes up
/// by name, so an older macOS just reports no per-window source.
pub(super) fn available() -> bool {
	AnyClass::get(c"SCShareableContent").is_some() && AnyClass::get(c"SCStream").is_some()
}

/// The current shareable content (on-screen windows only, desktop windows excluded).
fn shareable() -> anyhow::Result<Retained<SCShareableContent>> {
	if !available() {
		anyhow::bail!("ScreenCaptureKit requires macOS 12.3 or newer");
	}
	let (tx, rx) = mpsc::channel::<Result<SendBox<Retained<SCShareableContent>>, String>>();
	let tx = Mutex::new(Some(tx));
	let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
		let Some(tx) = tx.lock().unwrap().take() else { return };
		// SAFETY: the handler hands us +0 pointers valid for its duration; retain to keep.
		let res = match unsafe { Retained::retain(content) } {
			Some(c) => Ok(SendBox(c)),
			None => Err(unsafe { Retained::retain(err) }
				.map(|e| e.localizedDescription().to_string())
				.unwrap_or_else(|| "no shareable content".into())),
		};
		let _ = tx.send(res);
	});
	unsafe {
		SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
			true, true, &block,
		);
	}
	match rx.recv_timeout(SCK_TIMEOUT) {
		Ok(Ok(c)) => Ok(c.0),
		Ok(Err(e)) => anyhow::bail!("ScreenCaptureKit: {e}"),
		Err(_) => anyhow::bail!("ScreenCaptureKit did not answer in {}s", SCK_TIMEOUT.as_secs()),
	}
}

/// Backing scale (pixels per point) of the display that holds the window's centre; 1.0
/// when no display matches (the window straddles / is off every display).
fn backing_scale(content: &SCShareableContent, cx: f64, cy: f64) -> f64 {
	let displays = unsafe { content.displays() };
	for d in displays.iter() {
		let f = unsafe { d.frame() };
		let inside = cx >= f.origin.x
			&& cy >= f.origin.y
			&& cx < f.origin.x + f.size.width
			&& cy < f.origin.y + f.size.height;
		if !inside {
			continue;
		}
		let points = unsafe { d.width() }.max(1) as f64;
		let id = unsafe { d.displayID() };
		let pixels = core_graphics::display::CGDisplay::new(id)
			.display_mode()
			.map(|m| m.pixel_width() as f64)
			.unwrap_or(points);
		return (pixels / points).clamp(1.0, 4.0);
	}
	1.0
}

/// One window as a [`HostWindow`], or `None` when it fails the picker filter.
fn describe(content: &SCShareableContent, w: &SCWindow, own_pid: u32) -> Option<HostWindow> {
	if unsafe { w.windowLayer() } != 0 || !unsafe { w.isOnScreen() } {
		return None;
	}
	let app = unsafe { w.owningApplication() };
	let pid = app.as_ref().map(|a| unsafe { a.processID() }.max(0) as u32).unwrap_or(0);
	if pid == own_pid {
		return None;
	}
	let app_name = app.as_ref().map(|a| unsafe { a.applicationName() }.to_string()).unwrap_or_default();
	let title = unsafe { w.title() }.map(|t| t.to_string()).unwrap_or_default();
	let title = super::display_title(&title, &app_name)?;
	let f = unsafe { w.frame() };
	if f.size.width <= 1.0 || f.size.height <= 1.0 {
		return None;
	}
	let scale = backing_scale(content, f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
	Some(HostWindow {
		id: unsafe { w.windowID() } as u64,
		title,
		pid,
		width: (f.size.width * scale).round() as u32,
		height: (f.size.height * scale).round() as u32,
	})
}

pub(super) fn list() -> Vec<HostWindow> {
	objc2::rc::autoreleasepool(|_| {
		let content = match shareable() {
			Ok(c) => c,
			Err(e) => {
				tracing::debug!("window list: {e}");
				return Vec::new();
			}
		};
		let own = std::process::id();
		let windows = unsafe { content.windows() };
		windows.iter().filter_map(|w| describe(&content, &w, own)).collect()
	})
}

pub(super) fn size(id: u64) -> Option<(u32, u32)> {
	list().into_iter().find(|w| w.id == id).map(|w| (w.width, w.height))
}

/// `(pid, parent pid)` of every process (`proc_listallpids` + `PROC_PIDTBSDINFO`).
pub(super) fn process_edges() -> Vec<(u32, u32)> {
	let mut pids: Vec<libc::pid_t> = vec![0; 4096];
	let n = loop {
		let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
		// Returns the NUMBER of pids written (libproc divides the byte count).
		let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
		if n < 0 {
			return Vec::new();
		}
		let n = n as usize;
		if n < pids.len() || pids.len() >= 1 << 20 {
			break n.min(pids.len());
		}
		pids.resize(pids.len() * 2, 0);
	};
	pids[..n]
		.iter()
		.filter(|&&p| p > 0)
		.filter_map(|&p| {
			let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
			let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
			let got = unsafe {
				libc::proc_pidinfo(p, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
			};
			(got == size).then_some((p as u32, info.pbi_ppid))
		})
		.collect()
}

/// State shared by the ScreenCaptureKit output callback and the frame server.
struct Shared {
	width: usize,
	height: usize,
	/// The latest frame, packed NV12 (`width*height*3/2` bytes).
	frame: Mutex<Vec<u8>>,
	/// Frames received from ScreenCaptureKit (the server copies when it moves).
	received: AtomicU64,
	/// The stream stopped by itself (window closed, permission revoked) or ffmpeg left.
	dead: AtomicBool,
	/// [`WindowStream`] is being dropped.
	stop: AtomicBool,
}

impl Shared {
	/// Copy one NV12 `CVPixelBuffer` into `frame`.
	fn take(&self, sample: &CMSampleBuffer) {
		let Some(pb) = (unsafe { sample.image_buffer() }) else {
			// Idle / blank status frames carry no image.
			return;
		};
		if unsafe { CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) } != 0 {
			return;
		}
		let (w, h) = (self.width, self.height);
		{
			let mut dst = self.frame.lock().unwrap();
			let (y_dst, uv_dst) = dst.split_at_mut(w * h);
			for (plane, out, rows) in [(0usize, y_dst, h), (1usize, uv_dst, h / 2)] {
				let base = CVPixelBufferGetBaseAddressOfPlane(&pb, plane) as *const u8;
				if base.is_null() {
					continue;
				}
				let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, plane);
				let src_rows = CVPixelBufferGetHeightOfPlane(&pb, plane);
				// SAFETY: the plane is locked and spans `stride * src_rows` bytes.
				let src = unsafe { std::slice::from_raw_parts(base, stride * src_rows) };
				// Both planes are `w` bytes wide (UV interleaves w/2 Cb,Cr pairs).
				super::copy_plane(out, w, rows, src, stride, src_rows, w.min(stride));
			}
		}
		unsafe { CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) };
		self.received.fetch_add(1, Ordering::Release);
	}
}

define_class!(
	// The stream's sample output AND its delegate (to learn that it stopped).
	#[unsafe(super(NSObject))]
	#[name = "PulsarWindowCaptureOutput"]
	#[ivars = Arc<Shared>]
	struct Output;

	unsafe impl NSObjectProtocol for Output {}

	unsafe impl SCStreamOutput for Output {
		#[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
		unsafe fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
			if kind == SCStreamOutputType::Screen {
				self.ivars().take(sample);
			}
		}
	}

	unsafe impl SCStreamDelegate for Output {
		#[unsafe(method(stream:didStopWithError:))]
		unsafe fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
			tracing::warn!(error = %error.localizedDescription(), "ScreenCaptureKit window stream stopped");
			self.ivars().dead.store(true, Ordering::Release);
		}
	}
);

impl Output {
	fn new(shared: Arc<Shared>) -> Retained<Self> {
		let this = Self::alloc().set_ivars(shared);
		unsafe { msg_send![super(this), init] }
	}
}

/// One window being captured and served as raw NV12 on a loopback socket.
pub struct WindowStream {
	url: String,
	shared: Arc<Shared>,
	stream: SendBox<Retained<SCStream>>,
	_output: SendBox<Retained<Output>>,
	_queue: dispatch2::DispatchRetained<DispatchQueue>,
	server: Option<std::thread::JoinHandle<()>>,
}

impl WindowStream {
	/// Start capturing window `id` (a `CGWindowID`) at exactly `width`×`height` (even;
	/// the window is scaled into it keeping its aspect) and serve it at `fps`. The url
	/// ([`Self::url`]) serves one reader (ffmpeg) at a time; each must connect within 20 s
	/// of the previous one leaving.
	pub fn start(id: u64, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
		let (w, h) = ((width.max(2) & !1) as usize, (height.max(2) & !1) as usize);
		let fps = fps.clamp(1, 240);
		objc2::rc::autoreleasepool(|_| {
			let content = shareable()?;
			let windows = unsafe { content.windows() };
			let Some(window) = windows.iter().find(|x| unsafe { x.windowID() } as u64 == id) else {
				anyhow::bail!("window {id} is not on screen any more");
			};
			let filter =
				unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
			let config = unsafe { SCStreamConfiguration::new() };
			unsafe {
				config.setWidth(w);
				config.setHeight(h);
				config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
				config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
				config.setShowsCursor(true);
				config.setQueueDepth(3);
			}
			// macOS 14+: also scale a window SMALLER than the frame up, and keep its aspect
			// (12.3/13 only scale down and place it top-left). Looked up, not assumed: an
			// unknown selector would raise.
			if config.respondsToSelector(sel!(setScalesToFit:)) {
				let _: () = unsafe { msg_send![&config, setScalesToFit: true] };
			}
			if config.respondsToSelector(sel!(setPreservesAspectRatio:)) {
				let _: () = unsafe { msg_send![&config, setPreservesAspectRatio: true] };
			}

			let mut black = vec![16u8; w * h * 3 / 2];
			black[w * h..].fill(128);
			let shared = Arc::new(Shared {
				width: w,
				height: h,
				frame: Mutex::new(black),
				received: AtomicU64::new(0),
				dead: AtomicBool::new(false),
				stop: AtomicBool::new(false),
			});
			let output = Output::new(shared.clone());
			let delegate: &ProtocolObject<dyn SCStreamDelegate> = ProtocolObject::from_ref(&*output);
			let stream = unsafe {
				SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate))
			};
			let queue = DispatchQueue::new("com.pulsardesk.window-capture", None);
			let sink: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);
			unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Screen, Some(&queue)) }
				.map_err(|e| anyhow::anyhow!("ScreenCaptureKit addStreamOutput: {}", e.localizedDescription()))?;

			let (tx, rx) = mpsc::channel::<Option<String>>();
			let tx = Mutex::new(Some(tx));
			let done = RcBlock::new(move |err: *mut NSError| {
				if let Some(tx) = tx.lock().unwrap().take() {
					let msg = unsafe { Retained::retain(err) }.map(|e| e.localizedDescription().to_string());
					let _ = tx.send(msg);
				}
			});
			unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
			match rx.recv_timeout(SCK_TIMEOUT) {
				Ok(None) => {}
				Ok(Some(e)) => anyhow::bail!("ScreenCaptureKit start: {e}"),
				Err(_) => anyhow::bail!("ScreenCaptureKit start did not answer in {}s", SCK_TIMEOUT.as_secs()),
			}

			let listener = match TcpListener::bind(("127.0.0.1", 0)) {
				Ok(l) => l,
				Err(e) => {
					unsafe { stream.stopCaptureWithCompletionHandler(None) };
					return Err(e.into());
				}
			};
			let url = format!("tcp://127.0.0.1:{}", listener.local_addr()?.port());
			let srv_shared = shared.clone();
			let server = std::thread::Builder::new()
				.name("pulsar-window-frames".into())
				.spawn(move || serve(listener, srv_shared, fps))?;
			tracing::info!(id, w, h, fps, %url, "ScreenCaptureKit window capture started");
			Ok(WindowStream {
				url,
				shared,
				stream: SendBox(stream),
				_output: SendBox(output),
				_queue: queue,
				server: Some(server),
			})
		})
	}

	/// The loopback url ffmpeg reads the frames from (`tcp://127.0.0.1:PORT`).
	pub fn url(&self) -> &str {
		&self.url
	}

	/// The window stream still runs (not stopped by macOS, and a reader came in time).
	pub fn is_alive(&self) -> bool {
		!self.shared.dead.load(Ordering::Acquire)
	}

	/// Frames ScreenCaptureKit delivered so far (0 = nothing yet: no permission / hidden).
	pub fn frames_received(&self) -> u64 {
		self.shared.received.load(Ordering::Acquire)
	}

	/// Stop the capture and the frame server.
	pub fn stop(self) {}
}

impl Drop for WindowStream {
	fn drop(&mut self) {
		self.shared.stop.store(true, Ordering::Release);
		unsafe { self.stream.0.stopCaptureWithCompletionHandler(None) };
		if let Some(t) = self.server.take() {
			let _ = t.join();
		}
	}
}

/// Serve readers one after another: accept ffmpeg, write the latest frame every `1/fps`
/// until it leaves, then wait for the next one (the host's encoder fallback chain spawns a
/// new ffmpeg on the same url when one dies). Ends on stop, or when no reader comes.
fn serve(listener: TcpListener, shared: Arc<Shared>, fps: u32) {
	let mut buf = shared.frame.lock().unwrap().clone();
	let mut seen = 0u64;
	while !shared.stop.load(Ordering::Acquire) {
		let Some(mut sock) = accept(&listener, &shared) else {
			if !shared.stop.load(Ordering::Acquire) {
				shared.dead.store(true, Ordering::Release);
			}
			return;
		};
		let _ = sock.set_nodelay(true);
		// A reader that stops reading must not wedge the server (and `Drop`'s join).
		let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
		write_frames(&mut sock, &shared, fps, &mut buf, &mut seen);
	}
}

/// Write frames to one reader until it leaves or the stream stops.
fn write_frames(sock: &mut TcpStream, shared: &Shared, fps: u32, buf: &mut [u8], seen: &mut u64) {
	let tick = Duration::from_secs_f64(1.0 / fps as f64);
	let mut next = Instant::now();
	while !shared.stop.load(Ordering::Acquire) {
		let got = shared.received.load(Ordering::Acquire);
		if got != *seen {
			*seen = got;
			buf.copy_from_slice(&shared.frame.lock().unwrap());
		}
		if let Err(e) = sock.write_all(buf) {
			tracing::info!("window frame reader left: {e}");
			return;
		}
		next += tick;
		let now = Instant::now();
		if next > now {
			std::thread::sleep(next - now);
		} else if now - next > tick * 4 {
			// Fell far behind (the reader stalled): don't burst to catch up.
			next = now;
		}
	}
}

fn accept(listener: &TcpListener, shared: &Shared) -> Option<TcpStream> {
	listener.set_nonblocking(true).ok()?;
	let until = Instant::now() + ACCEPT_TIMEOUT;
	while Instant::now() < until && !shared.stop.load(Ordering::Acquire) {
		match listener.accept() {
			Ok((s, _)) => {
				s.set_nonblocking(false).ok()?;
				return Some(s);
			}
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
				std::thread::sleep(Duration::from_millis(20));
			}
			Err(e) => {
				tracing::warn!("window frame server accept: {e}");
				return None;
			}
		}
	}
	tracing::warn!("no reader connected to the window frame server");
	None
}
