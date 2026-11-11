//! Client side, USB-connected pad: turn host `Ds5Media::Slot`s into the PCM its own
//! 4-channel USB audio device plays (channels 1/2 speaker, 3/4 haptics, 48 kHz), and
//! pace that stream over a jittery network.
//!
//! One slot = one 10.667 ms BT haptics frame (32 samples at 3 kHz) = 512 frames at
//! 48 kHz. Its speaker part is a 10 ms Opus frame (480 frames, squeezed 512 → 480 by the
//! host for the BT pad, see `speaker`), so it is stretched back to 512 here. The app
//! decodes the Opus (codecs live in the apps) and owns the audio device.

use super::haptics::{Frame, Upsampler, FRAME_SAMPLES, RATIO, USB_CHANNELS};
use super::speaker::{IN_FRAMES, OUT_FRAMES};

/// 48 kHz frames per slot.
pub const SLOT_FRAMES: usize = FRAME_SAMPLES * RATIO;
const _: () = assert!(SLOT_FRAMES == IN_FRAMES);
/// Length of one slot in milliseconds.
pub const SLOT_MS: f32 = SLOT_FRAMES as f32 * 1000.0 / 48_000.0;

/// Linear-interpolate 480 stereo frames up to 512 (undoes `speaker::SpeakerFramer`'s
/// squeeze). Input shorter than 480 frames is zero-padded.
pub fn stretch(stereo480: &[f32]) -> Vec<f32> {
	let at = |i: usize, c: usize| stereo480.get(i * 2 + c).copied().unwrap_or(0.0);
	let mut o = Vec::with_capacity(IN_FRAMES * 2);
	let step = (OUT_FRAMES - 1) as f32 / (IN_FRAMES - 1) as f32;
	for i in 0..IN_FRAMES {
		let pos = i as f32 * step;
		let a = pos.floor() as usize;
		let b = (a + 1).min(OUT_FRAMES - 1);
		let t = pos - a as f32;
		for c in 0..2 {
			o.push(at(a, c) + (at(b, c) - at(a, c)) * t);
		}
	}
	o
}

/// Slot → 4-channel 48 kHz PCM for a USB pad.
#[derive(Debug, Default)]
pub struct UsbSlotMixer {
	up: Upsampler,
}

impl UsbSlotMixer {
	/// Append [`SLOT_FRAMES`] interleaved 4-channel frames: speaker (decoded 480-frame
	/// stereo, or silence) on 1/2, haptics (or silence) on 3/4.
	pub fn mix(&mut self, haptics: Option<&Frame>, speaker480: Option<&[f32]>, out: &mut Vec<f32>) {
		let start = out.len();
		match haptics {
			Some(h) => self.up.push_frame_4ch(h, out),
			None => {
				// Keep the interpolator continuous: ease from the last value to zero.
				self.up
					.push_frame_4ch(&[0; super::haptics::FRAME_BYTES], out);
			}
		}
		debug_assert_eq!(out.len() - start, SLOT_FRAMES * USB_CHANNELS);
		if let Some(spk) = speaker480 {
			let s = stretch(spk);
			for (i, f) in out[start..].chunks_exact_mut(USB_CHANNELS).enumerate() {
				f[0] = s[i * 2];
				f[1] = s[i * 2 + 1];
			}
		}
	}
}

/// What the player should do after queueing one slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pace {
	/// Keep (or put) the device paused: still prebuffering.
	Buffer,
	/// Play at this rate ratio (a small trim keeps the queue near the target).
	Play(f32),
	/// Too far behind: drop the queue and prebuffer again.
	Flush,
}

/// A small jitter buffer for the pad's audio stream, the USB twin of `Pad`'s BT FIFO:
/// start after `prebuffer_ms` queued, steer toward it with a ±1 % rate trim, rebuffer
/// after the queue ran dry, flush past `max_ms`.
#[derive(Debug, Clone)]
pub struct UsbPacer {
	prebuffer_ms: f32,
	max_ms: f32,
	playing: bool,
	pub underruns: u64,
	pub flushes: u64,
}

impl UsbPacer {
	pub fn new(prebuffer_ms: f32, max_ms: f32) -> Self {
		Self {
			prebuffer_ms,
			max_ms: max_ms.max(prebuffer_ms * 2.0),
			playing: false,
			underruns: 0,
			flushes: 0,
		}
	}

	pub fn playing(&self) -> bool {
		self.playing
	}

	/// `before_ms` = audio queued before this slot was added, `after_ms` = after.
	pub fn on_push(&mut self, before_ms: f32, after_ms: f32) -> Pace {
		if self.playing && before_ms <= 0.0 {
			// The device played silence: start over with a full prebuffer.
			self.underruns += 1;
			self.playing = false;
		}
		if after_ms > self.max_ms {
			self.flushes += 1;
			self.playing = false;
			return Pace::Flush;
		}
		if !self.playing {
			if after_ms < self.prebuffer_ms {
				return Pace::Buffer;
			}
			self.playing = true;
		}
		let ratio = if after_ms > self.prebuffer_ms * 1.5 {
			1.01
		} else if after_ms < self.prebuffer_ms * 0.5 {
			0.99
		} else {
			1.0
		};
		Pace::Play(ratio)
	}

	/// The stream ended (no slots for a while): pause and prebuffer next time.
	pub fn reset(&mut self) {
		self.playing = false;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stretch_is_512_frames_and_keeps_the_ends() {
		let s: Vec<f32> = (0..OUT_FRAMES)
			.flat_map(|i| [i as f32, -(i as f32)])
			.collect();
		let o = stretch(&s);
		assert_eq!(o.len(), IN_FRAMES * 2);
		assert_eq!((o[0], o[1]), (0.0, 0.0));
		let last = (OUT_FRAMES - 1) as f32;
		assert!((o[o.len() - 2] - last).abs() < 1e-3);
		assert!((o[o.len() - 1] + last).abs() < 1e-3);
		// Monotonic ramp stays monotonic.
		assert!(o
			.chunks_exact(2)
			.zip(o.chunks_exact(2).skip(1))
			.all(|(a, b)| b[0] >= a[0]));
		assert_eq!(stretch(&[]).len(), IN_FRAMES * 2, "short input → padded");
	}

	#[test]
	fn mixer_puts_speaker_on_1_2_and_haptics_on_3_4() {
		let mut m = UsbSlotMixer::default();
		let mut h = [0u8; super::super::haptics::FRAME_BYTES];
		for s in h.chunks_exact_mut(2) {
			s[0] = 127; // L full
			s[1] = (-127i8) as u8; // R full negative
		}
		let spk = vec![0.5f32; OUT_FRAMES * 2];
		let mut out = Vec::new();
		m.mix(Some(&h), Some(&spk), &mut out);
		assert_eq!(out.len(), SLOT_FRAMES * USB_CHANNELS);
		let f = &out[(SLOT_FRAMES - 1) * 4..];
		assert!((f[0] - 0.5).abs() < 1e-6 && (f[1] - 0.5).abs() < 1e-6);
		assert!((f[2] - 1.0).abs() < 1e-6 && (f[3] + 1.0).abs() < 1e-6);
		// No haptics, no speaker: silence after the easing slot.
		let mut out2 = Vec::new();
		m.mix(None, None, &mut out2);
		m.mix(None, None, &mut out2);
		assert!(out2[SLOT_FRAMES * 4..].iter().all(|&v| v == 0.0));
		assert!((SLOT_MS - 10.6667).abs() < 1e-3);
	}

	#[test]
	fn pacer_prebuffers_trims_rebuffers_and_flushes() {
		let mut p = UsbPacer::new(40.0, 160.0);
		assert_eq!(p.on_push(0.0, 10.7), Pace::Buffer);
		assert_eq!(p.on_push(10.7, 21.3), Pace::Buffer);
		assert_eq!(p.on_push(32.0, 42.7), Pace::Play(1.0));
		assert_eq!(
			p.on_push(60.0, 70.0),
			Pace::Play(1.01),
			"above target → faster"
		);
		assert_eq!(p.on_push(5.0, 15.0), Pace::Play(0.99), "below → slower");
		assert_eq!(p.on_push(0.0, 10.7), Pace::Buffer, "ran dry → rebuffer");
		assert_eq!(p.underruns, 1);
		assert_eq!(p.on_push(150.0, 170.0), Pace::Flush);
		assert_eq!(p.flushes, 1);
		assert!(!p.playing());
	}
}
