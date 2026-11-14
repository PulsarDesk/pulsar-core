//! macOS host capture through **ScreenCaptureKit**: the screen (NV12 frames, cursor
//! composited) and the system audio mix without Pulsar's own process.
//!
//! The old macOS host path is the ffmpeg CLI (`-f avfoundation` → `h264_videotoolbox` → RTP):
//! no live bitrate, no keyframe on request, and its audio default (`AvFoundation(0)`) is
//! usually the microphone — system audio needed a BlackHole-style virtual device. This module
//! is the capture half of the native path; the encoder (VideoToolbox) lives in the desktop
//! app's `pulsar-capture` crate like the Windows NVENC one, and the system-audio capture built
//! on it is [`crate::audio::run_system_audio_capture`].
//!
//! - [`sck::ScStream`]: an `SCStream` on one display — frames at the client's fps (macOS
//!   12.3+), and/or the system audio with this process left out (`excludesCurrentProcessAudio`,
//!   macOS 13+).
//! - [`sck::displays`]: the display list in the host's index order (the main display first).
//! - [`sck::screen_capture_access`]: the Screen Recording (TCC) permission.
//!
//! **Nothing here links ScreenCaptureKit.** The framework is opened at run time and every
//! symbol newer than the app's minimum macOS is looked up dynamically, so the app still starts
//! on an older Mac and the host keeps its ffmpeg path there.
//!
//! Untested on macOS hardware: written and type-checked on another OS
//! (`cargo check --target aarch64-apple-darwin`); the first real run may need fixes.

/// PCM reshaping for the SCK audio output. Pure, so it builds and is unit-tested everywhere.
pub mod pcm;

use std::sync::atomic::{AtomicU32, Ordering};

/// The `CGDirectDisplayID` the host is streaming right now (0 = none published: the main
/// display). The capture publishes it ([`set_streamed_display`]) and the macOS input injector
/// maps an absolute (normalized) pointer onto that display's bounds, so a click lands on the
/// screen the client sees, not always the main one. Process-wide: two sessions streaming
/// different displays at once map onto the one published last.
static STREAMED_DISPLAY: AtomicU32 = AtomicU32::new(0);

/// Publish the display that is being streamed (see [`streamed_display`]).
pub fn set_streamed_display(display_id: u32) {
	STREAMED_DISPLAY.store(display_id, Ordering::SeqCst);
}

/// Withdraw `display_id` when its capture ends — only if it still is the published one, so
/// an ending capture never clears a newer capture's display.
pub fn clear_streamed_display(display_id: u32) {
	let _ = STREAMED_DISPLAY.compare_exchange(display_id, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// The display being streamed (0 = none published: use the main display).
pub fn streamed_display() -> u32 {
	STREAMED_DISPLAY.load(Ordering::SeqCst)
}

/// A normalized (0..1) position on a display placed at `left, top` with `width × height`
/// points, in global display points (the CGEvent space). Out-of-range input is clamped onto
/// the display.
pub fn map_to_display(x: f64, y: f64, left: f64, top: f64, width: f64, height: f64) -> (f64, f64) {
	(left + x.clamp(0.0, 1.0) * width, top + y.clamp(0.0, 1.0) * height)
}

/// The ffmpeg avfoundation screen number (`"Capture screen N"`) of a display: its place in
/// CoreGraphics' active display list (the order ffmpeg enumerates screens in), or `None`.
pub fn avfoundation_screen_number(active_ids: &[u32], display_id: u32) -> Option<u32> {
	active_ids.iter().position(|&id| id == display_id).map(|p| p as u32)
}

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
pub mod sck;

#[cfg(target_os = "macos")]
pub use sck::{
	active_display_ids, displays, sck_available, screen_capture_access, AudioSpec, MacDisplay, ScError, ScStream, StreamSpec, VideoFrame,
	VideoSpec,
};

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_normalized_positions_onto_the_display() {
		// A second display right of a 1512-pt main one.
		assert_eq!(map_to_display(0.0, 0.0, 1512.0, 0.0, 1920.0, 1080.0), (1512.0, 0.0));
		assert_eq!(map_to_display(0.5, 0.5, 1512.0, 0.0, 1920.0, 1080.0), (2472.0, 540.0));
		// A display above the main one has a negative origin; input is clamped onto it.
		assert_eq!(map_to_display(1.5, -0.2, 0.0, -1080.0, 1920.0, 1080.0), (1920.0, -1080.0));
	}

	#[test]
	fn avfoundation_numbers_screens_in_active_list_order() {
		let ids = [69733378, 2, 4];
		assert_eq!(avfoundation_screen_number(&ids, 69733378), Some(0));
		assert_eq!(avfoundation_screen_number(&ids, 4), Some(2));
		assert_eq!(avfoundation_screen_number(&ids, 7), None);
	}

	#[test]
	fn clearing_keeps_a_newer_display() {
		set_streamed_display(5);
		set_streamed_display(6);
		clear_streamed_display(5);
		assert_eq!(streamed_display(), 6);
		clear_streamed_display(6);
		assert_eq!(streamed_display(), 0);
	}
}
