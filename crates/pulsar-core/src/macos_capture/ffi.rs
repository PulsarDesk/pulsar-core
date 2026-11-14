//! Raw bindings for the few CoreFoundation / CoreMedia / CoreGraphics / libdispatch calls the
//! ScreenCaptureKit capture needs. Hand-written (not the objc2-* framework crates) on purpose:
//! those link every framework strongly, and ScreenCaptureKit does not exist before macOS 12.3
//! — a strong link would stop the whole app from launching on an older Mac. Everything newer
//! than the app's minimum macOS is resolved at run time through [`sym`].

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void, CStr};

use objc2::Encode;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFIndex = isize;
pub type OSStatus = i32;
pub type Boolean = u8;
pub type CMSampleBufferRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMFormatDescriptionRef = *mut c_void;
pub type CVPixelBufferRef = *mut c_void;
pub type CGDirectDisplayID = u32;
pub type CGDisplayModeRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGPoint {
	pub x: f64,
	pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGSize {
	pub width: f64,
	pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGRect {
	pub origin: CGPoint,
	pub size: CGSize,
}

/// `CMTime` (`{?=qiIq}`): value / timescale seconds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CMTime {
	pub value: i64,
	pub timescale: i32,
	pub flags: u32,
	pub epoch: i64,
}

/// `kCMTimeFlags_Valid`.
pub const CMTIME_VALID: u32 = 1;

impl CMTime {
	pub fn new(value: i64, timescale: i32) -> Self {
		Self { value, timescale, flags: CMTIME_VALID, epoch: 0 }
	}
}

// SAFETY: same layout and encoding as CoreMedia's anonymous `CMTime` struct.
unsafe impl Encode for CMTime {
	const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
		"?",
		&[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
	);
}


#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamBasicDescription {
	pub mSampleRate: f64,
	pub mFormatID: u32,
	pub mFormatFlags: u32,
	pub mBytesPerPacket: u32,
	pub mFramesPerPacket: u32,
	pub mBytesPerFrame: u32,
	pub mChannelsPerFrame: u32,
	pub mBitsPerChannel: u32,
	pub mReserved: u32,
}

/// `kAudioFormatLinearPCM` ('lpcm').
pub const kAudioFormatLinearPCM: u32 = 0x6c70_636d;
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsSignedInteger: u32 = 1 << 2;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBuffer {
	pub mNumberChannels: u32,
	pub mDataByteSize: u32,
	pub mData: *mut c_void,
}

/// An `AudioBufferList` with room for 16 buffers (one per channel of a planar format).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBufferList16 {
	pub mNumberBuffers: u32,
	pub mBuffers: [AudioBuffer; 16],
}

impl Default for AudioBufferList16 {
	fn default() -> Self {
		Self {
			mNumberBuffers: 0,
			mBuffers: [AudioBuffer { mNumberChannels: 0, mDataByteSize: 0, mData: std::ptr::null_mut() }; 16],
		}
	}
}

/// `kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment`.
pub const kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment: u32 = 1;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v', NV12 video range).
pub const PIXEL_FORMAT_NV12_VIDEO: u32 = 0x3432_3076;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
	pub fn CFRelease(cf: CFTypeRef);
	pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
	pub fn CFArrayGetValueAtIndex(a: CFArrayRef, idx: CFIndex) -> *const c_void;
	pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
	pub fn CFNumberGetValue(n: CFTypeRef, the_type: CFIndex, value_ptr: *mut c_void) -> Boolean;
	pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
	pub fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
	pub fn CFStringGetCString(s: CFStringRef, buffer: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
}

/// `kCFStringEncodingUTF8`.
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

/// A CFString (or a toll-free bridged NSString) as a Rust String; empty on failure.
pub unsafe fn cf_string_to_string(s: CFStringRef) -> String {
	if s.is_null() {
		return String::new();
	}
	let len = CFStringGetLength(s);
	let cap = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
	if cap <= 1 {
		return String::new();
	}
	let mut buf = vec![0u8; cap as usize];
	if CFStringGetCString(s, buf.as_mut_ptr().cast(), cap, kCFStringEncodingUTF8) == 0 {
		return String::new();
	}
	CStr::from_bytes_until_nul(&buf).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `kCFNumberSInt64Type`.
pub const kCFNumberSInt64Type: CFIndex = 4;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
	pub fn CMSampleBufferIsValid(sbuf: CMSampleBufferRef) -> Boolean;
	pub fn CMSampleBufferDataIsReady(sbuf: CMSampleBufferRef) -> Boolean;
	pub fn CMSampleBufferGetImageBuffer(sbuf: CMSampleBufferRef) -> CVPixelBufferRef;
	pub fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CMSampleBufferRef, create_if_necessary: Boolean) -> CFArrayRef;
	pub fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
	pub fn CMSampleBufferGetNumSamples(sbuf: CMSampleBufferRef) -> CFIndex;
	pub fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
		sbuf: CMSampleBufferRef,
		buffer_list_size_needed_out: *mut usize,
		buffer_list_out: *mut AudioBufferList16,
		buffer_list_size: usize,
		block_buffer_structure_allocator: *const c_void,
		block_buffer_block_allocator: *const c_void,
		flags: u32,
		block_buffer_out: *mut CMBlockBufferRef,
	) -> OSStatus;
	pub fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: CMFormatDescriptionRef) -> *const AudioStreamBasicDescription;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
	pub fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
	pub fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
	pub fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
	pub fn CGMainDisplayID() -> CGDirectDisplayID;
	pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
	pub fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
	pub fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
	pub fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
	pub fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
	pub fn CGDisplayModeRelease(mode: CGDisplayModeRef);
	pub fn CGDisplayIsBuiltin(display: CGDirectDisplayID) -> u32;
}

// libdispatch lives in libSystem (always linked).
extern "C" {
	pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
	pub fn dispatch_queue_attr_make_with_qos_class(attr: *const c_void, qos: u32, relative_priority: i32) -> *const c_void;
	pub fn dispatch_release(object: *mut c_void);
}

/// `QOS_CLASS_USER_INTERACTIVE`.
pub const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

/// Look a symbol up in every image loaded so far (`RTLD_DEFAULT`).
pub fn sym(name: &CStr) -> Option<*mut c_void> {
	// SAFETY: dlsym with a NUL-terminated name; a missing symbol is a null return.
	let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
	(!p.is_null()).then_some(p)
}

/// The value of an exported `CFStringRef` constant (`extern const CFStringRef name;`), or
/// `None` when this macOS does not have it.
pub fn cf_string_const(name: &CStr) -> Option<CFStringRef> {
	// SAFETY: the symbol is the address of a CFStringRef variable.
	sym(name).map(|p| unsafe { *(p as *const CFStringRef) }).filter(|s| !s.is_null())
}

/// Open ScreenCaptureKit (macOS 12.3+). `false` on an older macOS.
pub fn load_screencapturekit() -> bool {
	static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*LOADED.get_or_init(|| {
		let path = c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit";
		// SAFETY: dlopen of a system framework path; the handle is kept for the process' life.
		let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
		!h.is_null()
	})
}

/// `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess` (macOS 10.15+).
pub fn screen_capture_access_fn(request: bool) -> Option<extern "C" fn() -> bool> {
	let name = if request { c"CGRequestScreenCaptureAccess" } else { c"CGPreflightScreenCaptureAccess" };
	// SAFETY: both functions take no arguments and return a C `bool`.
	sym(name).map(|p| unsafe { std::mem::transmute::<*mut c_void, extern "C" fn() -> bool>(p) })
}

/// Read a CFNumber as i64.
pub fn cf_number_i64(n: CFTypeRef) -> Option<i64> {
	if n.is_null() {
		return None;
	}
	let mut v: i64 = 0;
	// SAFETY: `n` is a CFNumber (the caller read it from a dictionary documented to hold one).
	let ok = unsafe { CFNumberGetValue(n, kCFNumberSInt64Type, (&mut v as *mut i64).cast()) };
	(ok != 0).then_some(v)
}

