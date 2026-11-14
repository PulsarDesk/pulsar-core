//! macOS system audio through ScreenCaptureKit (macOS 13+): the mix of every app with
//! Pulsar's own process left out (`excludesCurrentProcessAudio`) — the macOS counterpart of
//! Windows' process loopback in `Exclude` mode. It needs no virtual audio device (BlackHole &
//! co.) and is tapped before the output device, so it does not depend on which output is the
//! default; it needs the Screen Recording permission (SCK has no audio-only grant).
//!
//! Output: interleaved 48 kHz stereo f32 little-endian PCM ([`system_audio_format`]), written to
//! a [`Write`] sink — the same contract as the Windows captures, so the desktop app feeds it to
//! its in-process Opus/L16 RTP encoder (or an ffmpeg reading `-f f32le -ar 48000 -ac 2`).
//!
//! Untested on macOS hardware.

use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::macos_capture::sck::{AudioSpec, ScError, ScStream, StreamSpec};

/// The PCM layout [`run_system_audio_capture`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemAudioFormat {
	pub rate: u32,
	pub channels: u16,
}

impl SystemAudioFormat {
	/// The ffmpeg `-f` name of the samples.
	pub fn ffmpeg_sample_fmt(&self) -> &'static str {
		"f32le"
	}
}

/// 48 kHz stereo f32 — what [`run_system_audio_capture`] writes.
pub fn system_audio_format() -> SystemAudioFormat {
	SystemAudioFormat { rate: super::SAMPLE_RATE, channels: super::CHANNELS }
}

/// Whether this Mac can capture the system audio through ScreenCaptureKit at all (macOS 13+
/// with SCK present). Says nothing about the permission.
pub fn system_audio_supported() -> bool {
	crate::macos_capture::sck_available() && macos_13_or_newer()
}

fn macos_13_or_newer() -> bool {
	// `kern.osproductversion` ("13.4.1"); unknown = assume yes and let the start fail.
	let mut buf = [0u8; 32];
	let mut len = buf.len();
	// SAFETY: sysctlbyname into a local buffer of `len` bytes.
	let r = unsafe {
		libc::sysctlbyname(
			c"kern.osproductversion".as_ptr(),
			buf.as_mut_ptr().cast(),
			&mut len,
			std::ptr::null_mut(),
			0,
		)
	};
	if r != 0 {
		return true;
	}
	let s = String::from_utf8_lossy(&buf[..len.saturating_sub(1).min(buf.len())]).to_string();
	s.split('.').next().and_then(|m| m.trim_matches(char::from(0)).parse::<u32>().ok()).is_none_or(|m| m >= 13)
}

/// Silence is written after this long without a buffer from SCK, so the encoder's timeline
/// keeps moving while nothing plays (SCK may stop sending buffers then).
const IDLE_FILL_AFTER: Duration = Duration::from_millis(60);
/// Silence is written in steps of this many frames (10 ms).
const FILL_FRAMES: usize = 480;

/// Capture the system mix (all apps but this one) and write 48 kHz stereo f32le PCM to `sink`
/// until a write fails (the encoder retired / the pipe broke — a clean `Ok(())`) or the stream
/// dies (`Err`). **Blocking** — run it on its own thread. `started` runs once the stream is
/// live, before the first write, so a caller waiting for it can fall back when this returns an
/// `Err` first. `display_id` picks the display the stream is attached to (0 = main; the audio
/// is the same for every display).
pub fn run_system_audio_capture<W: Write>(mut sink: W, display_id: u32, started: impl FnOnce()) -> Result<(), String> {
	if !system_audio_supported() {
		return Err("system audio capture needs macOS 13 or newer".into());
	}
	let fmt = system_audio_format();
	// Bounded: a stuck writer drops audio instead of growing memory without end.
	let (tx, rx) = mpsc::sync_channel::<Option<Vec<f32>>>(256);
	let stop_tx = tx.clone();
	let stream = ScStream::start(
		StreamSpec {
			display_id,
			video: None,
			audio: Some(AudioSpec { sample_rate: fmt.rate, channels: fmt.channels as u32 }),
		},
		None,
		Some(Box::new(move |pcm: &[f32]| {
			let _ = tx.try_send(Some(pcm.to_vec()));
		})),
		Some(Box::new(move |_why: String| {
			let _ = stop_tx.try_send(None);
		})),
	)
	.map_err(|e| match e {
		ScError::PermissionDenied => "screen recording permission denied (needed for system audio)".to_string(),
		e => e.to_string(),
	})?;
	started();
	let silence = vec![0u8; FILL_FRAMES * fmt.channels as usize * 4];
	let mut bytes: Vec<u8> = Vec::with_capacity(8192);
	let mut last_audio = Instant::now();
	let mut last_fill = Instant::now();
	let result = loop {
		match rx.recv_timeout(Duration::from_millis(10)) {
			Ok(Some(pcm)) => {
				bytes.clear();
				bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
				if sink.write_all(&bytes).is_err() {
					break Ok(());
				}
				last_audio = Instant::now();
			}
			Ok(None) => break Err("ScreenCaptureKit stopped the audio stream".to_string()),
			Err(mpsc::RecvTimeoutError::Timeout) => {}
			Err(mpsc::RecvTimeoutError::Disconnected) => break Err("audio stream ended".to_string()),
		}
		// Idle: keep the timeline moving with 10 ms of silence per 10 ms of wall clock.
		if last_audio.elapsed() >= IDLE_FILL_AFTER && last_fill.elapsed() >= Duration::from_millis(10) {
			if sink.write_all(&silence).is_err() {
				break Ok(());
			}
			last_fill = Instant::now();
		}
	};
	stream.stop();
	result
}
