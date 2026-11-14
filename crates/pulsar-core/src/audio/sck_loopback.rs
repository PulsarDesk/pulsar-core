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
/// At most this many frames of silence go out in one write (100 ms at 48 kHz), so a long stall
/// of this thread does not turn into one huge burst.
const MAX_FILL_FRAMES: u64 = 4800;

/// Frames of silence still owed `elapsed` after the idle fill began, when `written` frames of
/// it went out already: the wall clock times the rate, minus what was written, capped at
/// [`MAX_FILL_FRAMES`]. Based on the wall clock, so the silence keeps exact real-time pace
/// however late the 10 ms loop wakes up.
fn silence_frames_due(elapsed: Duration, written: u64, rate: u32) -> u64 {
	let owed = (elapsed.as_nanos() * rate as u128 / 1_000_000_000) as u64;
	owed.saturating_sub(written).min(MAX_FILL_FRAMES)
}

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
	let frame_bytes = fmt.channels as usize * 4;
	let silence = vec![0u8; MAX_FILL_FRAMES as usize * frame_bytes];
	let mut bytes: Vec<u8> = Vec::with_capacity(8192);
	let mut last_audio = Instant::now();
	// Frames of silence written since the current idle stretch began (reset by real audio).
	let mut filled: u64 = 0;
	let result = loop {
		match rx.recv_timeout(Duration::from_millis(10)) {
			Ok(Some(pcm)) => {
				bytes.clear();
				bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
				if sink.write_all(&bytes).is_err() {
					break Ok(());
				}
				last_audio = Instant::now();
				filled = 0;
			}
			Ok(None) => break Err("ScreenCaptureKit stopped the audio stream".to_string()),
			Err(mpsc::RecvTimeoutError::Timeout) => {}
			Err(mpsc::RecvTimeoutError::Disconnected) => break Err("audio stream ended".to_string()),
		}
		// Idle: keep the timeline moving with silence at the wall clock's pace, counted from
		// the moment the idle fill began (the first IDLE_FILL_AFTER of a gap is not filled, so
		// a short pause between SCK buffers never doubles up with the real audio).
		let idle = last_audio.elapsed();
		if idle >= IDLE_FILL_AFTER {
			let due = silence_frames_due(idle - IDLE_FILL_AFTER, filled, fmt.rate);
			if due > 0 {
				if sink.write_all(&silence[..due as usize * frame_bytes]).is_err() {
					break Ok(());
				}
				filled += due;
			}
		}
	};
	stream.stop();
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn silence_follows_the_wall_clock() {
		// 10 ms at 48 kHz = 480 frames owed, none written yet.
		assert_eq!(silence_frames_due(Duration::from_millis(10), 0, 48_000), 480);
		// A late wake-up (25 ms) after 480 frames: the missing 720 go out now, no drift.
		assert_eq!(silence_frames_due(Duration::from_millis(25), 480, 48_000), 720);
		// Already ahead (or exactly on time): nothing owed.
		assert_eq!(silence_frames_due(Duration::from_millis(10), 480, 48_000), 0);
		assert_eq!(silence_frames_due(Duration::from_millis(5), 480, 48_000), 0);
		// A long stall is capped per write; the rest follows on the next loop turns.
		assert_eq!(silence_frames_due(Duration::from_secs(2), 0, 48_000), MAX_FILL_FRAMES);
	}
}
