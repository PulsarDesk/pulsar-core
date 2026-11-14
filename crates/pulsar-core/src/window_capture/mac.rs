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

use super::{FitPrev, HostWindow, WindowRect};

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

/// Window `id`'s bounds (global display points, top-left origin) and its owner's pid,
/// straight from the window server: no ScreenCaptureKit round trip, and these keys need
/// no Screen Recording grant. Cheap enough for the input path to poll.
fn window_info(id: u64) -> Option<(WindowRect, i32)> {
	use core_foundation::base::{CFType, TCFType};
	use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
	use core_foundation::number::CFNumber;
	use core_graphics::geometry::CGRect;
	use core_graphics::window::{
		copy_window_info, kCGWindowBounds, kCGWindowListOptionIncludingWindow, kCGWindowNumber, kCGWindowOwnerPID,
	};
	use std::ffi::c_void;
	let wid = u32::try_from(id).ok()?;
	let list = copy_window_info(kCGWindowListOptionIncludingWindow, wid)?;
	let number = |dict: &CFDictionary, key: *const c_void| -> Option<i64> {
		let v = dict.find(key)?;
		// SAFETY: a CFDictionary value is a CF object; retained by the wrapper.
		unsafe { CFType::wrap_under_get_rule(*v) }.downcast::<CFNumber>()?.to_i64()
	};
	for item in list.iter() {
		// SAFETY: CGWindowListCopyWindowInfo returns an array of CFDictionaries.
		let dict: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
		if number(&dict, unsafe { kCGWindowNumber } as *const c_void) != Some(id as i64) {
			continue;
		}
		let bounds = dict.find(unsafe { kCGWindowBounds } as *const c_void)?;
		let bounds: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*bounds as CFDictionaryRef) };
		let r = CGRect::from_dict_representation(&bounds)?;
		let pid = number(&dict, unsafe { kCGWindowOwnerPID } as *const c_void).unwrap_or(0) as i32;
		return Some((WindowRect { x: r.origin.x, y: r.origin.y, w: r.size.width, h: r.size.height }, pid));
	}
	None
}

pub(super) fn rect(id: u64) -> Option<WindowRect> {
	window_info(id).map(|(r, _)| r)
}

/// Height kept free at the top of every display for the menu bar when a window is fitted
/// (macOS shows one per display by default; 37 pt on a notched panel). CoreGraphics has no
/// public "visible frame", and NSScreen is main-thread-only.
const MENU_BAR_PT: f64 = 38.0;

/// Each active display's area a fitted window may use (global points).
fn display_areas() -> Vec<WindowRect> {
	let ids = core_graphics::display::CGDisplay::active_displays().unwrap_or_default();
	ids.into_iter()
		.map(|id| {
			let b = core_graphics::display::CGDisplay::new(id).bounds();
			let inset = MENU_BAR_PT.min(b.size.height / 4.0);
			WindowRect { x: b.origin.x, y: b.origin.y + inset, w: b.size.width, h: b.size.height - inset }
		})
		.collect()
}

/// Activate the app owning `pid` (all its windows come forward), like a click on its Dock
/// icon. NSRunningApplication is looked up at runtime: AppKit is loaded in the desktop app
/// (Tauri) but not necessarily in a bare pulsar-core binary, which then just skips this.
fn activate_app(pid: i32) {
	use objc2::runtime::AnyObject;
	let Some(cls) = AnyClass::get(c"NSRunningApplication") else { return };
	objc2::rc::autoreleasepool(|_| {
		let app: *mut AnyObject = unsafe { msg_send![cls, runningApplicationWithProcessIdentifier: pid] };
		if let Some(app) = unsafe { app.as_ref() } {
			// NSApplicationActivateIgnoringOtherApps (1 << 1): come forward even though the
			// user is "in" another app.
			let _: bool = unsafe { msg_send![app, activateWithOptions: 2usize] };
		}
	});
}

pub(super) fn raise(id: u64) {
	let Some((r, pid)) = window_info(id) else { return };
	if pid > 0 {
		activate_app(pid);
	}
	// The app's OTHER windows may still cover this one: raise it and make it the app's
	// main (key) window through Accessibility (only with the grant; prompts nothing).
	if ax::trusted() {
		if let Some(w) = ax::window_for(pid, r) {
			ax::raise(&w);
		}
	}
}

/// Wait (up to ~0.5 s) for the app to apply a new size to window `id`.
fn wait_for_size(id: u64, from: WindowRect) {
	let until = Instant::now() + Duration::from_millis(500);
	while Instant::now() < until {
		match rect(id) {
			Some(r) if (r.w, r.h) != (from.w, from.h) => return,
			None => return,
			_ => std::thread::sleep(Duration::from_millis(25)),
		}
	}
}

pub(super) fn fit(id: u64, pane_w: u32, pane_h: u32) -> Option<FitPrev> {
	if !ax::trusted() {
		tracing::info!(id, "window-fit: no Accessibility permission, the window keeps its size");
		return None;
	}
	let (before, pid) = window_info(id)?;
	let win = ax::window_for(pid, before)?;
	if ax::flag(&win, "AXFullScreen") {
		tracing::info!(id, "window-fit: the window is full-screen, left as it is");
		return None;
	}
	let work = super::area_of(&before, &display_areas())?;
	// The whole window (title bar included) is what ScreenCaptureKit streams, so the whole
	// window takes the pane's shape. Points, not pixels: the stream is scaled into the
	// client's box anyway.
	let (w, h) = super::fit_client(pane_w, pane_h, work.w.max(1.0) as u32, work.h.max(1.0) as u32);
	let (w, h) = (w as f64, h as f64);
	let x = super::place(before.x, w, work.x, work.w);
	let y = super::place(before.y, h, work.y, work.h);
	if !ax::set_frame(&win, WindowRect { x, y, w, h }) {
		tracing::warn!(id, "window-fit: the app refused the new window frame");
		return None;
	}
	wait_for_size(id, before);
	tracing::info!(id, pane = %format!("{pane_w}x{pane_h}"), size = %format!("{w}x{h}"), "window-fit: resized the macOS window to the split pane's shape");
	Some(FitPrev { id, rect: before, maximized: (false, false) })
}

pub(super) fn restore(prev: &FitPrev) {
	let Some((now, pid)) = window_info(prev.id) else {
		return; // closed meanwhile — nothing to restore
	};
	if let Some(w) = ax::window_for(pid, now) {
		ax::set_frame(&w, prev.rect);
		wait_for_size(prev.id, now);
		tracing::info!(id = prev.id, "window-fit: restored the macOS window");
	}
}

/// The Accessibility (AX) API: the only public way to move / resize / raise ANOTHER app's
/// window on macOS. Needs the Accessibility grant (System Settings → Privacy & Security →
/// Accessibility), the same one remote input already needs for `CGEventPost`.
mod ax {
	use std::ffi::c_void;

	use core_foundation::base::{CFType, CFTypeRef, TCFType};
	use core_foundation::boolean::CFBoolean;
	use core_foundation::string::{CFString, CFStringRef};
	use core_graphics::geometry::{CGPoint, CGSize};

	use super::super::WindowRect;

	type AXError = i32;
	const AX_OK: AXError = 0;
	const AX_VALUE_CGPOINT: u32 = 1;
	const AX_VALUE_CGSIZE: u32 = 2;

	#[link(name = "ApplicationServices", kind = "framework")]
	extern "C" {
		fn AXIsProcessTrusted() -> u8;
		fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
		fn AXUIElementCopyAttributeValue(element: CFTypeRef, attribute: CFStringRef, value: *mut CFTypeRef) -> AXError;
		fn AXUIElementSetAttributeValue(element: CFTypeRef, attribute: CFStringRef, value: CFTypeRef) -> AXError;
		fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> AXError;
		fn AXValueCreate(ty: u32, value: *const c_void) -> CFTypeRef;
		fn AXValueGetValue(value: CFTypeRef, ty: u32, out: *mut c_void) -> u8;
	}

	/// This process holds the Accessibility grant (never prompts).
	pub(super) fn trusted() -> bool {
		unsafe { AXIsProcessTrusted() != 0 }
	}

	fn attr(element: &CFType, name: &'static str) -> Option<CFType> {
		let key = CFString::from_static_string(name);
		let mut out: CFTypeRef = std::ptr::null();
		let err = unsafe { AXUIElementCopyAttributeValue(element.as_CFTypeRef(), key.as_concrete_TypeRef(), &mut out) };
		// SAFETY: a "Copy" function: the value comes +1.
		(err == AX_OK && !out.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(out) })
	}

	/// A boolean attribute (`false` when unset / not a boolean).
	pub(super) fn flag(element: &CFType, name: &'static str) -> bool {
		attr(element, name).and_then(|v| v.downcast::<CFBoolean>()).is_some_and(bool::from)
	}

	fn point(element: &CFType, name: &'static str) -> Option<(f64, f64)> {
		let v = attr(element, name)?;
		let mut p = CGPoint::new(0.0, 0.0);
		(unsafe { AXValueGetValue(v.as_CFTypeRef(), AX_VALUE_CGPOINT, (&mut p as *mut CGPoint).cast()) } != 0)
			.then_some((p.x, p.y))
	}

	fn size(element: &CFType, name: &'static str) -> Option<(f64, f64)> {
		let v = attr(element, name)?;
		let mut s = CGSize::new(0.0, 0.0);
		(unsafe { AXValueGetValue(v.as_CFTypeRef(), AX_VALUE_CGSIZE, (&mut s as *mut CGSize).cast()) } != 0)
			.then_some((s.width, s.height))
	}

	fn set(element: &CFType, name: &'static str, ty: u32, value: *const c_void) -> bool {
		let v = unsafe { AXValueCreate(ty, value) };
		if v.is_null() {
			return false;
		}
		// SAFETY: AXValueCreate returns +1.
		let v = unsafe { CFType::wrap_under_create_rule(v) };
		let key = CFString::from_static_string(name);
		unsafe { AXUIElementSetAttributeValue(element.as_CFTypeRef(), key.as_concrete_TypeRef(), v.as_CFTypeRef()) == AX_OK }
	}

	/// Move + resize: position, size, then position again (an app that clamps the window to
	/// its screen during the resize would otherwise end up off the wanted origin).
	pub(super) fn set_frame(element: &CFType, r: WindowRect) -> bool {
		let p = CGPoint::new(r.x, r.y);
		let s = CGSize::new(r.w, r.h);
		let pos = |e: &CFType| set(e, "AXPosition", AX_VALUE_CGPOINT, (&p as *const CGPoint).cast());
		let moved = pos(element);
		let sized = set(element, "AXSize", AX_VALUE_CGSIZE, (&s as *const CGSize).cast());
		pos(element);
		moved || sized
	}

	pub(super) fn raise(element: &CFType) {
		let action = CFString::from_static_string("AXRaise");
		unsafe { AXUIElementPerformAction(element.as_CFTypeRef(), action.as_concrete_TypeRef()) };
		let yes = CFBoolean::true_value();
		let key = CFString::from_static_string("AXMain");
		unsafe { AXUIElementSetAttributeValue(element.as_CFTypeRef(), key.as_concrete_TypeRef(), yes.as_CFTypeRef()) };
	}

	/// The AX window of app `pid` whose frame matches the window server's `r` (AX has no
	/// public CGWindowID: match on position + size, the closest within a few points).
	pub(super) fn window_for(pid: i32, r: WindowRect) -> Option<CFType> {
		if pid <= 0 {
			return None;
		}
		let app = unsafe { AXUIElementCreateApplication(pid) };
		if app.is_null() {
			return None;
		}
		// SAFETY: a "Create" function: +1.
		let app = unsafe { CFType::wrap_under_create_rule(app) };
		let windows = attr(&app, "AXWindows")?.downcast::<core_foundation::array::CFArray>()?;
		let mut best: Option<(f64, CFType)> = None;
		for item in windows.iter() {
			// SAFETY: AXWindows is an array of AXUIElements.
			let w = unsafe { CFType::wrap_under_get_rule(*item) };
			let (Some((x, y)), Some((ww, hh))) = (point(&w, "AXPosition"), size(&w, "AXSize")) else { continue };
			let d = super::super::frame_distance(r, WindowRect { x, y, w: ww, h: hh });
			if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
				best = Some((d, w));
			}
		}
		best.filter(|(d, _)| *d <= 8.0).map(|(_, w)| w)
	}
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
	/// The frame size (pixels, even) the stream is configured for.
	size: (u32, u32),
	shared: Arc<Shared>,
	stream: SendBox<Retained<SCStream>>,
	_output: SendBox<Retained<Output>>,
	_queue: dispatch2::DispatchRetained<DispatchQueue>,
	server: Option<std::thread::JoinHandle<()>>,
}

impl WindowStream {
	/// Start capturing window `id` (a `CGWindowID`), its pixel size fitted into the `max`
	/// box keeping its aspect (never upscaled, even — [`Self::size`]), and serve it at
	/// `fps`. The url ([`Self::url`]) serves one reader (ffmpeg) at a time; each must
	/// connect within 20 s of the previous one leaving. Blocking (see `serve_for`).
	pub fn start(id: u64, max: (u32, u32), fps: u32) -> anyhow::Result<Self> {
		let fps = fps.clamp(1, 240);
		objc2::rc::autoreleasepool(|_| {
			let content = shareable()?;
			let windows = unsafe { content.windows() };
			let Some(window) = windows.iter().find(|x| unsafe { x.windowID() } as u64 == id) else {
				anyhow::bail!("window {id} is not on screen any more");
			};
			// The window's own pixel size from THIS query (no second listing for it).
			let f = unsafe { window.frame() };
			let scale = backing_scale(&content, f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
			let px = ((f.size.width * scale).round().max(2.0) as u32, (f.size.height * scale).round().max(2.0) as u32);
			let (fw, fh) = crate::pipeline::fit_within(px, max);
			let (w, h) = ((fw.max(2) & !1) as usize, (fh.max(2) & !1) as usize);
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
				size: (w as u32, h as u32),
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

	/// The frame size every served frame has (`width`, `height`).
	pub fn size(&self) -> (u32, u32) {
		self.size
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
