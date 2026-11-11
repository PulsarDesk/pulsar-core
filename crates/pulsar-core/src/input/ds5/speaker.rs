//! Pad speaker audio over Bluetooth: framing before the Opus encoder (which lives in the
//! apps, like every codec).
//!
//! The pad plays one 10 ms Opus frame (48 kHz stereo CELT, CBR 160 kbit/s = 200 B) per
//! 10.667 ms haptics slot. ZerkSense5 and DS5Dongle therefore squeeze every 512 input
//! frames into 480 (16:15) before encoding; this compresses the content by 6.25 % and
//! is the accepted behaviour of every known implementation.

/// Input stereo frames per haptics slot (10.667 ms at 48 kHz).
pub const IN_FRAMES: usize = 512;
/// Stereo frames in one 10 ms Opus frame at 48 kHz.
pub const OUT_FRAMES: usize = 480;
/// Opus bitrate that yields exactly 200 bytes per 10 ms frame.
pub const OPUS_BITRATE: i32 = 160_000;

/// 48 kHz stereo f32 in, 480-frame blocks (interleaved L, R) out, one per 512 frames in.
#[derive(Debug, Default)]
pub struct SpeakerFramer {
	pending: Vec<f32>,
}

impl SpeakerFramer {
	/// Feed interleaved stereo samples; each complete block (960 f32) goes to `out`.
	pub fn push(&mut self, stereo: &[f32], out: &mut Vec<Vec<f32>>) {
		self.pending.extend_from_slice(stereo);
		while self.pending.len() >= IN_FRAMES * 2 {
			let block: Vec<f32> = self.pending.drain(..IN_FRAMES * 2).collect();
			out.push(squeeze(&block));
		}
	}
}

/// Linear-interpolate 512 stereo frames down to 480.
fn squeeze(block: &[f32]) -> Vec<f32> {
	let mut o = Vec::with_capacity(OUT_FRAMES * 2);
	let step = (IN_FRAMES - 1) as f32 / (OUT_FRAMES - 1) as f32;
	for i in 0..OUT_FRAMES {
		let pos = i as f32 * step;
		let a = pos.floor() as usize;
		let b = (a + 1).min(IN_FRAMES - 1);
		let t = pos - a as f32;
		for c in 0..2 {
			o.push(block[a * 2 + c] * (1.0 - t) + block[b * 2 + c] * t);
		}
	}
	o
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_block_per_512_frames_and_endpoints_kept() {
		let mut f = SpeakerFramer::default();
		let mut out = Vec::new();
		let input: Vec<f32> = (0..IN_FRAMES)
			.flat_map(|i| [i as f32, -(i as f32)])
			.collect();
		f.push(&input[..1000], &mut out);
		assert!(out.is_empty());
		f.push(&input[1000..], &mut out);
		assert_eq!(out.len(), 1);
		let b = &out[0];
		assert_eq!(b.len(), OUT_FRAMES * 2);
		assert_eq!((b[0], b[1]), (0.0, 0.0));
		assert!(
			(b[b.len() - 2] - 511.0).abs() < 1e-3,
			"last input frame kept"
		);
		assert!(b.windows(2).step_by(2).all(|w| w[0] >= 0.0 && w[1] <= 0.0));
	}
}
