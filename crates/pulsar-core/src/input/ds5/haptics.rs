//! HD haptics sample handling.
//!
//! A wired DualSense takes haptics as channels 3/4 of its 4-channel 48 kHz USB audio
//! device. Over Bluetooth it takes 3 kHz signed 8-bit stereo in 64-byte frames
//! (32 stereo samples = 10.667 ms). This module converts between the two.
//!
//! Reference behaviour (ZerkSense5 `LinearResampler`, DS5Dongle) is plain 16:1
//! decimation with no anti-alias filter; DS5Dongle notes the wired pad does no filtering
//! either. [`Downsampler::with_filter`] adds an optional box average over each 16-sample
//! window (a cheap low-pass) — off by default to match the reference.

/// Bluetooth haptics sample rate.
pub const BT_RATE: u32 = 3000;
/// USB audio sample rate.
pub const USB_RATE: u32 = 48_000;
/// USB samples per BT sample.
pub const RATIO: usize = (USB_RATE / BT_RATE) as usize;
/// Stereo samples per BT haptics frame.
pub const FRAME_SAMPLES: usize = 32;
/// Bytes per BT haptics frame (interleaved L, R, int8).
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// One BT haptics frame (interleaved L, R signed 8-bit).
pub type Frame = [u8; FRAME_BYTES];
/// Channels of the USB audio stream; haptics are the last two.
pub const USB_CHANNELS: usize = 4;

/// `x` in -1.0..=1.0 → signed 8-bit stored as `u8` (two's complement).
fn to_s8(x: f32) -> u8 {
	((x * 127.0).round().clamp(-128.0, 127.0) as i8) as u8
}

/// 48 kHz 4-channel USB PCM (channels 3/4) → 3 kHz s8 stereo frames.
#[derive(Debug)]
pub struct Downsampler {
	gain: f32,
	filter: bool,
	/// Position inside the current 16-sample window.
	phase: usize,
	acc: [f32; 2],
	frame: Frame,
	fill: usize,
}

impl Default for Downsampler {
	fn default() -> Self {
		Self::new(1.0)
	}
}

impl Downsampler {
	/// `gain` multiplies the haptics signal before quantising (ZerkSense allows 1.0..=2.0).
	pub fn new(gain: f32) -> Self {
		Self {
			gain,
			filter: false,
			phase: 0,
			acc: [0.0; 2],
			frame: [0; FRAME_BYTES],
			fill: 0,
		}
	}

	/// Average each 16-sample window instead of picking one sample.
	pub fn with_filter(mut self, on: bool) -> Self {
		self.filter = on;
		self
	}

	/// Feed interleaved 4-channel s16 frames; complete BT frames are pushed to `out`.
	pub fn push_usb_s16(&mut self, pcm: &[i16], out: &mut Vec<Frame>) {
		for f in pcm.chunks_exact(USB_CHANNELS) {
			self.push_pair(f[2] as f32 / 32768.0, f[3] as f32 / 32768.0, out);
		}
	}

	/// Feed haptics-only stereo samples at 48 kHz (`[l, r]` pairs, -1.0..=1.0).
	pub fn push_pair(&mut self, l: f32, r: f32, out: &mut Vec<Frame>) {
		if self.filter {
			self.acc[0] += l;
			self.acc[1] += r;
		} else if self.phase == 0 {
			self.acc = [l, r];
		}
		self.phase += 1;
		if self.phase < RATIO {
			return;
		}
		self.phase = 0;
		let (l, r) = if self.filter {
			(self.acc[0] / RATIO as f32, self.acc[1] / RATIO as f32)
		} else {
			(self.acc[0], self.acc[1])
		};
		self.acc = [0.0; 2];
		self.frame[self.fill] = to_s8(l * self.gain);
		self.frame[self.fill + 1] = to_s8(r * self.gain);
		self.fill += 2;
		if self.fill == FRAME_BYTES {
			out.push(self.frame);
			self.fill = 0;
		}
	}
}

/// 3 kHz s8 stereo frames → 48 kHz haptics pairs (for a USB-connected client pad,
/// whose audio device takes haptics as channels 3/4). Linear interpolation.
#[derive(Debug, Default)]
pub struct Upsampler {
	last: [f32; 2],
}

impl Upsampler {
	/// Append 48 kHz 4-channel f32 frames (channels 1/2 silent) for one BT frame.
	pub fn push_frame_4ch(&mut self, frame: &Frame, out: &mut Vec<f32>) {
		for s in frame.chunks_exact(2) {
			let next = [s[0] as i8 as f32 / 127.0, s[1] as i8 as f32 / 127.0];
			for k in 1..=RATIO {
				let t = k as f32 / RATIO as f32;
				out.extend_from_slice(&[
					0.0,
					0.0,
					self.last[0] + (next[0] - self.last[0]) * t,
					self.last[1] + (next[1] - self.last[1]) * t,
				]);
			}
			self.last = next;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn usb_block(n: usize, ch3: i16, ch4: i16) -> Vec<i16> {
		(0..n).flat_map(|_| [1000, -1000, ch3, ch4]).collect()
	}

	#[test]
	fn one_frame_per_512_usb_samples() {
		let mut d = Downsampler::default();
		let mut out = Vec::new();
		d.push_usb_s16(&usb_block(511, 16384, -16384), &mut out);
		assert!(out.is_empty(), "511 samples are not a full frame yet");
		d.push_usb_s16(&usb_block(1, 16384, -16384), &mut out);
		assert_eq!(out.len(), 1, "32 BT samples × 16 = 512 USB samples");
		let f = out[0];
		assert_eq!(f[0] as i8, 64, "0.5 × 127 rounds to 64");
		assert_eq!(
			f[1] as i8, -64,
			"right channel is channel 4, speaker channels ignored"
		);
	}

	#[test]
	fn gain_and_clipping() {
		let mut d = Downsampler::new(2.0);
		let mut out = Vec::new();
		d.push_usb_s16(&usb_block(512, i16::MAX, i16::MIN), &mut out);
		assert_eq!(out[0][0] as i8, 127);
		assert_eq!(out[0][1] as i8, -128);
	}

	#[test]
	fn filter_averages_the_window() {
		// A full-scale 24 kHz square (alternating samples) aliases to DC without a filter
		// and averages to ~0 with one.
		let pcm: Vec<i16> = (0..512)
			.flat_map(|i| {
				let v = if i % 2 == 0 { 32767 } else { -32767 };
				[0, 0, v, v]
			})
			.collect();
		let mut raw = Vec::new();
		Downsampler::default().push_usb_s16(&pcm, &mut raw);
		let mut filt = Vec::new();
		Downsampler::default()
			.with_filter(true)
			.push_usb_s16(&pcm, &mut filt);
		assert_eq!(raw[0][0] as i8, 127, "unfiltered decimation picks the peak");
		assert_eq!(filt[0][0] as i8, 0, "box average cancels it");
	}

	#[test]
	fn upsample_is_continuous_and_4ch() {
		let mut f = [0u8; FRAME_BYTES];
		for s in f.chunks_exact_mut(2) {
			s[0] = 127;
			s[1] = (-127i8) as u8;
		}
		let mut u = Upsampler::default();
		let mut out = Vec::new();
		u.push_frame_4ch(&f, &mut out);
		assert_eq!(out.len(), FRAME_SAMPLES * RATIO * USB_CHANNELS);
		// First 48 kHz sample ramps from 0 towards 1.0.
		assert!(out[2] > 0.0 && out[2] < 0.1);
		assert_eq!(out[0], 0.0, "speaker channels silent");
		// After the first BT sample the signal sits at full scale.
		let last = &out[out.len() - 4..];
		assert!((last[2] - 1.0).abs() < 1e-6 && (last[3] + 1.0).abs() < 1e-6);
	}
}
