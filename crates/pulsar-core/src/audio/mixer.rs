//! Sums several live captures of the same format (48 kHz stereo f32, interleaved) into one
//! stream: the host's per-app audio, when some apps are left out of the session and every other
//! app is captured on its own (`DataMsg::AudioApps`).
//!
//! Each source pushes whatever its capture delivered; a paced reader [`Mixer::pull`]s fixed
//! blocks. A source joins the sum once it holds [`PRIME_MS`] (a cushion for its capture's
//! jitter), sits out while it runs dry, and drops its oldest audio past [`MAX_MS`] (its
//! capture's clock runs ahead of the reader's), so no source can drift into latency.

use std::collections::{HashMap, VecDeque};

/// Channels per frame.
pub const MIX_CHANNELS: usize = 2;
/// Frames per millisecond at 48 kHz.
const FRAMES_PER_MS: usize = 48;
/// Buffered audio a source needs before it is summed (and again after it ran dry).
pub const PRIME_MS: usize = 20;
/// More than this buffered: the oldest is dropped.
pub const MAX_MS: usize = 120;

#[derive(Default)]
struct Source {
	buf: VecDeque<f32>,
	primed: bool,
}

/// See the module docs. Source ids are the caller's (a process id, say).
#[derive(Default)]
pub struct Mixer {
	srcs: HashMap<u64, Source>,
}

impl Mixer {
	pub fn new() -> Mixer {
		Mixer::default()
	}

	/// `samples` (interleaved stereo) captured by source `id`.
	pub fn push(&mut self, id: u64, samples: &[f32]) {
		let s = self.srcs.entry(id).or_default();
		s.buf.extend(samples.iter().copied());
		let max = MAX_MS * FRAMES_PER_MS * MIX_CHANNELS;
		if s.buf.len() > max {
			let over = (s.buf.len() - max).next_multiple_of(MIX_CHANNELS).min(s.buf.len());
			s.buf.drain(..over);
		}
		if s.buf.len() >= PRIME_MS * FRAMES_PER_MS * MIX_CHANNELS {
			s.primed = true;
		}
	}

	/// Forget source `id` (its capture ended).
	pub fn remove(&mut self, id: u64) {
		self.srcs.remove(&id);
	}

	/// The sources pushed and not removed.
	pub fn sources(&self) -> impl Iterator<Item = u64> + '_ {
		self.srcs.keys().copied()
	}

	/// The next `frames` frames: every primed source summed (clamped to −1..1); silence where
	/// none plays.
	pub fn pull(&mut self, frames: usize) -> Vec<f32> {
		let n = frames * MIX_CHANNELS;
		let mut out = vec![0f32; n];
		for s in self.srcs.values_mut() {
			if !s.primed {
				continue;
			}
			let take = n.min(s.buf.len());
			for (o, v) in out.iter_mut().zip(s.buf.drain(..take)) {
				*o += v;
			}
			if take < n {
				// Ran dry mid-block: wait for a fresh cushion instead of stuttering on every block.
				s.primed = false;
			}
		}
		for o in out.iter_mut() {
			*o = o.clamp(-1.0, 1.0);
		}
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ms(v: f32, ms: usize) -> Vec<f32> {
		vec![v; ms * FRAMES_PER_MS * MIX_CHANNELS]
	}

	#[test]
	fn sums_primed_sources_only() {
		let mut m = Mixer::new();
		m.push(1, &ms(0.25, 30));
		m.push(2, &ms(0.5, 5)); // not primed yet
		let out = m.pull(480);
		assert!(out.iter().all(|v| (*v - 0.25).abs() < 1e-6));
		m.push(2, &ms(0.5, 20));
		let out = m.pull(480);
		assert!(out.iter().all(|v| (*v - 0.75).abs() < 1e-6), "{}", out[0]);
	}

	#[test]
	fn a_dry_source_sits_out_until_primed_again() {
		let mut m = Mixer::new();
		m.push(1, &ms(0.5, 20));
		let _ = m.pull(960); // 20 ms: drained exactly
		let out = m.pull(480); // dry
		assert!(out.iter().all(|v| *v == 0.0));
		m.push(1, &ms(0.5, 10)); // under the cushion
		assert!(m.pull(480).iter().all(|v| *v == 0.0));
		m.push(1, &ms(0.5, 15));
		assert!(m.pull(480).iter().all(|v| (*v - 0.5).abs() < 1e-6));
	}

	#[test]
	fn backlog_is_bounded_and_the_sum_clamped() {
		let mut m = Mixer::new();
		m.push(1, &ms(0.9, 500));
		m.push(2, &ms(0.9, 30));
		assert!(m.srcs[&1].buf.len() <= MAX_MS * FRAMES_PER_MS * MIX_CHANNELS);
		assert_eq!(m.srcs[&1].buf.len() % MIX_CHANNELS, 0);
		assert!(m.pull(480).iter().all(|v| *v == 1.0));
		m.remove(1);
		assert_eq!(m.sources().collect::<Vec<_>>(), vec![2]);
	}
}
