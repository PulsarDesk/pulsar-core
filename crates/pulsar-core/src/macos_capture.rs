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

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
pub mod sck;

#[cfg(target_os = "macos")]
pub use sck::{
	displays, sck_available, screen_capture_access, AudioSpec, MacDisplay, ScError, ScStream, StreamSpec, VideoFrame,
	VideoSpec,
};
