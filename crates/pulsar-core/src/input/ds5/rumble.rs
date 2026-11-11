//! Classic rumble made from HD haptics frames, for when haptics are shed on a weak
//! Bluetooth link (`degrade.rs`): the game keeps vibrating the pad, just coarser.
//!
//! Same idea as ZerkSense5's RumbleEngine/HapticsMixer, the other way round: an envelope
//! follower over each 3 kHz haptics channel drives a rumble motor (left channel → heavy
//! left motor, right channel → light right motor).

use std::time::{Duration, Instant};

use super::haptics::Frame;

/// Envelope decay per 10.667 ms frame (~45 ms to half).
const RELEASE: f32 = 0.85;
/// Envelope → motor gain (haptics rarely run at full scale).
const GAIN: f32 = 1.6;
/// Below this envelope the motor stays still (noise, idle hiss).
const GATE: f32 = 0.05;
/// Motor levels are quantised to this step so a jittery envelope doesn't flood the link.
const STEP: u8 = 16;
/// No frame for this long = the haptics stream stopped.
pub const STALE: Duration = Duration::from_millis(60);

#[derive(Debug, Default)]
pub struct HapticsRumble {
	env: [f32; 2],
	last: Option<Instant>,
}

impl HapticsRumble {
	/// One haptics frame (interleaved L, R signed 8-bit) arrived at `now`.
	pub fn feed(&mut self, f: &Frame, now: Instant) {
		let mut peak = [0f32; 2];
		for (i, &b) in f.iter().enumerate() {
			let x = (b as i8 as f32).abs() / 127.0;
			peak[i & 1] = peak[i & 1].max(x);
		}
		for c in 0..2 {
			self.env[c] = peak[c].max(self.env[c] * RELEASE);
		}
		self.last = Some(now);
	}

	/// Motor levels `(heavy/left, light/right)` at `now`; zero once the stream is stale.
	pub fn level(&self, now: Instant) -> (u8, u8) {
		let live = self
			.last
			.is_some_and(|t| now.saturating_duration_since(t) < STALE);
		if !live {
			return (0, 0);
		}
		let m = |e: f32| -> u8 {
			if e < GATE {
				return 0;
			}
			let v = ((e * GAIN).min(1.0) * 255.0) as u8;
			(v / STEP * STEP).max(STEP)
		};
		(m(self.env[0]), m(self.env[1]))
	}

	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::input::ds5::haptics::FRAME_BYTES;

	fn frame(l: i8, r: i8) -> Frame {
		let mut f = [0u8; FRAME_BYTES];
		for i in 0..FRAME_BYTES / 2 {
			let s = if i % 2 == 0 { 1 } else { -1 };
			f[2 * i] = (l as i16 * s).clamp(-128, 127) as i8 as u8;
			f[2 * i + 1] = (r as i16 * s).clamp(-128, 127) as i8 as u8;
		}
		f
	}

	#[test]
	fn envelope_maps_channels_to_motors() {
		let t = Instant::now();
		let mut h = HapticsRumble::default();
		assert_eq!(h.level(t), (0, 0));
		h.feed(&frame(60, 0), t);
		let (heavy, light) = h.level(t);
		assert!(heavy >= 176, "{heavy}");
		assert_eq!(light, 0);
		h.feed(&frame(0, 40), t);
		let (heavy2, light2) = h.level(t);
		assert!(heavy2 < heavy && heavy2 > 0, "decays, {heavy2}");
		assert!(light2 > 64, "{light2}");
		assert_eq!(light2 % STEP, 0);
	}

	#[test]
	fn silence_and_stale_stream_stop_the_motors() {
		let t = Instant::now();
		let mut h = HapticsRumble::default();
		h.feed(&frame(3, 3), t);
		assert_eq!(h.level(t), (0, 0), "below the gate");
		h.feed(&frame(120, 120), t);
		assert_ne!(h.level(t), (0, 0));
		assert_eq!(h.level(t + STALE), (0, 0));
	}
}
