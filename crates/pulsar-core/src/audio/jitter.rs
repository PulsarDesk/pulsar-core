//! Jitter buffer + adaptive drift-correcting resampler for a capture that does not run in
//! real time: the host-silent virtual sink's loopback (Steam Streaming Speakers) delivers
//! 46.8k–56.4k frames/s in bursts instead of a steady 48k (`docs/agents/audio.md` in
//! pulsar-desktop). The host pushes whatever the capture delivers; a wall-clock pacer pulls
//! a steady 48 kHz out.
//!
//! - **Target fill** [`TARGET_MS`] (30 ms: the added latency). Output starts once the
//!   buffer holds it (prebuffer, faded in). Each underrun raises the target by 5 ms, up to
//!   [`TARGET_MAX_MS`]; 30 s without one lowers it by 5 ms again, so a burstier source buys
//!   only the latency it needs.
//! - **Ratio trim**: the read step is `base × (1 + P)`. `base` is the source's measured mean
//!   rate over a few seconds ÷ 48 kHz (limited to ±3 %): it grows only when the source
//!   really runs fast or slow, and — unlike an integrator — can't wind up on bursts or
//!   underruns. P (±0.5 %) pulls the smoothed fill back to the target.
//! - **Overflow** past the target + [`HEADROOM_MS`] (a burst): drop back to the target, crossfaded over 1 ms.
//! - **Underrun**: the last sample fades out over 2 ms, then silence until the prebuffer is
//!   back (an idle endpoint delivers nothing at all).
//!
//! Stereo, 48 kHz, f32 interleaved in and out. Linear interpolation (the ratio stays within a
//! few percent of 1, so its high-frequency loss is small).

use std::collections::VecDeque;

const RATE: f64 = 48_000.0;
/// Fill the controller holds (ms) — the added latency.
pub const TARGET_MS: f64 = 30.0;
/// The target never grows past this (ms).
pub const TARGET_MAX_MS: f64 = 60.0;
/// Above target + this (ms) the excess is dropped back to the target.
pub const HEADROOM_MS: f64 = 60.0;
const TARGET_STEP_MS: f64 = 5.0;
/// Underrun-free time after which the target steps back down (s).
const TARGET_RELAX_S: f64 = 30.0;
/// Proportional trim limit and gain (ratio per second of fill error).
const P_MAX: f64 = 0.005;
const KP: f64 = 0.5;
/// Limit of the measured-rate base ratio.
const BASE_MAX: f64 = 0.03;
/// Time constant of the source-rate estimate (s).
const RATE_TAU: f64 = 4.0;
/// Time constant of the fill average the controller reads (s).
const FILL_TAU: f64 = 0.25;
/// Frames of the fades.
const FADE_IN: usize = 96;
const FADE_OUT: usize = 96;
const XFADE: usize = 48;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct JitterStats {
	pub underruns: u64,
	pub overflows: u64,
	pub dropped_frames: u64,
}

pub struct DriftResampler {
	buf: VecDeque<[f32; 2]>,
	/// Fractional read position past `buf[0]` (0..1).
	pos: f64,
	primed: bool,
	target: f64,
	base_target: f64,
	max_target: f64,
	high: f64,
	/// Output frames since the last underrun or target step-down.
	calm: u64,
	avg_fill: f64,
	/// Frames pushed since the last pull, and the source-rate estimate (frames/s).
	pushed: u64,
	in_rate: f64,
	ratio: f64,
	/// Output gain ramp after (re)priming.
	fade_in: usize,
	/// Frames left of the fade-out of `last` after an underrun.
	fade_out: usize,
	last: [f32; 2],
	/// Frames from before an overflow drop, mixed into the first output frames after it.
	xfade: Vec<[f32; 2]>,
	xfade_at: usize,
	stats: JitterStats,
}

impl Default for DriftResampler {
	fn default() -> Self {
		Self::new()
	}
}

impl DriftResampler {
	pub fn new() -> Self {
		let target = TARGET_MS / 1000.0 * RATE;
		Self {
			buf: VecDeque::with_capacity(
				((TARGET_MAX_MS + HEADROOM_MS) / 1000.0 * RATE) as usize * 2,
			),
			pos: 0.0,
			primed: false,
			target,
			base_target: target,
			max_target: TARGET_MAX_MS / 1000.0 * RATE,
			high: target + HEADROOM_MS / 1000.0 * RATE,
			calm: 0,
			avg_fill: target,
			pushed: 0,
			in_rate: RATE,
			ratio: 1.0,
			fade_in: FADE_IN,
			fade_out: 0,
			last: [0.0; 2],
			xfade: Vec::new(),
			xfade_at: 0,
			stats: JitterStats::default(),
		}
	}

	/// Interleaved stereo f32 frames as the capture delivered them.
	pub fn push(&mut self, interleaved: &[f32]) {
		for f in interleaved.chunks_exact(2) {
			self.buf.push_back([f[0], f[1]]);
		}
		self.pushed += (interleaved.len() / 2) as u64;
	}

	/// Frames buffered (the latency right now, in frames).
	pub fn fill(&self) -> f64 {
		(self.buf.len() as f64 - self.pos).max(0.0)
	}

	/// The fill held now (ms).
	pub fn target_ms(&self) -> f64 {
		self.target / RATE * 1000.0
	}

	pub fn ratio(&self) -> f64 {
		self.ratio
	}

	pub fn primed(&self) -> bool {
		self.primed
	}

	pub fn stats(&self) -> JitterStats {
		self.stats
	}

	/// Append `frames` stereo frames (interleaved) of steady 48 kHz output to `out`.
	pub fn pull(&mut self, frames: usize, out: &mut Vec<f32>) {
		self.control(frames);
		if self.primed && self.fill() > self.high {
			self.drop_to_target();
		}
		out.reserve(frames * 2);
		for _ in 0..frames {
			let s = self.next_frame();
			out.push(s[0]);
			out.push(s[1]);
		}
	}

	/// The trim, once per pull (`frames` of wall time).
	fn control(&mut self, frames: usize) {
		let dt = frames as f64 / RATE;
		self.calm += frames as u64;
		if self.calm as f64 >= TARGET_RELAX_S * RATE && self.target > self.base_target {
			self.set_target(self.target - TARGET_STEP_MS / 1000.0 * RATE);
		}
		let a = 1.0 - (-dt / FILL_TAU).exp();
		self.avg_fill += (self.fill() - self.avg_fill) * a;
		let inst = std::mem::take(&mut self.pushed) as f64 / dt;
		self.in_rate += (inst - self.in_rate) * (1.0 - (-dt / RATE_TAU).exp());
		if !self.primed {
			return;
		}
		let base = (self.in_rate / RATE).clamp(1.0 - BASE_MAX, 1.0 + BASE_MAX);
		let err = (self.avg_fill - self.target) / RATE; // seconds
		let p = (KP * err).clamp(-P_MAX, P_MAX);
		self.ratio = base * (1.0 + p);
	}

	fn set_target(&mut self, t: f64) {
		self.target = t.clamp(self.base_target, self.max_target);
		self.high = self.target + HEADROOM_MS / 1000.0 * RATE;
		self.calm = 0;
	}

	fn drop_to_target(&mut self) {
		// Remember what would have played, to crossfade from it into the post-drop audio.
		self.xfade.clear();
		let mut p = self.pos;
		let mut i = 0usize;
		while self.xfade.len() < XFADE && i + 1 < self.buf.len() {
			self.xfade
				.push(lerp(self.buf[i], self.buf[i + 1], p as f32));
			p += self.ratio;
			while p >= 1.0 {
				p -= 1.0;
				i += 1;
			}
		}
		self.xfade_at = 0;
		let excess = (self.fill() - self.target) as usize;
		self.buf.drain(..excess.min(self.buf.len()));
		self.avg_fill = self.fill();
		self.stats.overflows += 1;
		self.stats.dropped_frames += excess as u64;
	}

	fn next_frame(&mut self) -> [f32; 2] {
		if !self.primed {
			if self.buf.len() as f64 >= self.target {
				// `avg_fill` is NOT reset here: after an underrun it still remembers the
				// buffer ran dry, so the trim comes down instead of cycling into the next one.
				self.primed = true;
				self.fade_in = 0;
			} else {
				return self.fade_tail();
			}
		}
		if self.buf.len() < 2 {
			// Underrun: fade the last sample out, wait for the prebuffer.
			self.primed = false;
			self.fade_out = FADE_OUT;
			self.stats.underruns += 1;
			self.set_target(self.target + TARGET_STEP_MS / 1000.0 * RATE);
			return self.fade_tail();
		}
		let mut s = lerp(self.buf[0], self.buf[1], self.pos as f32);
		if self.xfade_at < self.xfade.len() {
			let t = (self.xfade_at + 1) as f32 / (self.xfade.len() + 1) as f32;
			let o = self.xfade[self.xfade_at];
			s = [o[0] * (1.0 - t) + s[0] * t, o[1] * (1.0 - t) + s[1] * t];
			self.xfade_at += 1;
		}
		if self.fade_in < FADE_IN {
			let g = (self.fade_in + 1) as f32 / FADE_IN as f32;
			s = [s[0] * g, s[1] * g];
			self.fade_in += 1;
		}
		self.pos += self.ratio;
		while self.pos >= 1.0 && !self.buf.is_empty() {
			self.pos -= 1.0;
			self.buf.pop_front();
		}
		self.last = s;
		s
	}

	/// Silence, after the fade-out of the last sample played.
	fn fade_tail(&mut self) -> [f32; 2] {
		if self.fade_out == 0 {
			return [0.0; 2];
		}
		self.fade_out -= 1;
		let g = self.fade_out as f32 / FADE_OUT as f32;
		[self.last[0] * g, self.last[1] * g]
	}
}

fn lerp(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
	[a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Tiny deterministic PRNG (xorshift) for the burst pattern.
	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> f64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			(self.0 >> 11) as f64 / (1u64 << 53) as f64
		}
	}

	/// A 440 Hz tone rendered at a source clock whose rate wanders over `rates` (frames/s,
	/// each held for `segment_s`), delivered in bursts every 5–30 ms; the output is pulled
	/// in 5 ms steps on an exact 48 kHz wall clock. Returns (output, fills sampled per pull).
	fn simulate(
		rs: &mut DriftResampler,
		rates: &[f64],
		segment_s: f64,
		seconds: f64,
		seed: u64,
	) -> (Vec<f32>, Vec<f64>) {
		let mut rng = Rng(seed);
		let mut out = Vec::new();
		let mut fills = Vec::new();
		let mut phase = 0.0f64;
		let mut src_due = 0.0f64; // source frames owed so far
		let mut next_burst = 0.0f64;
		let step = 0.005;
		let mut t = 0.0;
		let mut pending: Vec<f32> = Vec::new();
		while t < seconds {
			let rate = rates[((t / segment_s) as usize) % rates.len()];
			src_due += rate * step;
			while src_due >= 1.0 {
				let v = (phase * std::f64::consts::TAU).sin() as f32 * 0.5;
				pending.push(v);
				pending.push(v);
				phase = (phase + 440.0 / 48_000.0).fract();
				src_due -= 1.0;
			}
			if t >= next_burst {
				rs.push(&pending);
				pending.clear();
				next_burst = t + 0.005 + rng.next() * 0.025;
			}
			rs.pull(240, &mut out);
			fills.push(rs.fill() / 48.0);
			t += step;
		}
		(out, fills)
	}

	fn max_step(out: &[f32]) -> f32 {
		out.chunks_exact(2)
			.zip(out.chunks_exact(2).skip(1))
			.map(|(a, b)| (a[0] - b[0]).abs())
			.fold(0.0, f32::max)
	}

	// Natural max step of a 0.5 × 440 Hz tone: 2π·440/48000·0.5 ≈ 0.029.
	const CLICK: f32 = 0.06;

	#[test]
	fn steady_48k_with_jitter_holds_the_target_without_clicks() {
		let mut rs = DriftResampler::new();
		let (out, fills) = simulate(&mut rs, &[48_000.0], 1.0, 20.0, 7);
		let tail = &fills[fills.len() / 2..];
		let mean = tail.iter().sum::<f64>() / tail.len() as f64;
		assert!((mean - TARGET_MS).abs() < 8.0, "mean fill {mean} ms");
		assert_eq!(rs.stats().overflows, 0, "{:?}", rs.stats());
		assert!(rs.stats().underruns <= 1, "{:?}", rs.stats());
		assert_eq!(rs.target_ms(), TARGET_MS, "no reason to grow");
		assert!((rs.ratio() - 1.0).abs() < 0.003, "ratio {}", rs.ratio());
		assert!(
			max_step(&out[48_000 * 2..]) < CLICK,
			"click {}",
			max_step(&out)
		);
	}

	#[test]
	fn a_source_running_two_percent_fast_is_absorbed_by_the_ratio() {
		let mut rs = DriftResampler::new();
		let (out, fills) = simulate(&mut rs, &[49_000.0], 1.0, 40.0, 3);
		// No drift growth: the last 10 s stay near the target, no drops once settled.
		let late = &fills[fills.len() * 3 / 4..];
		let max = late.iter().cloned().fold(0.0, f64::max);
		let mean = late.iter().sum::<f64>() / late.len() as f64;
		assert!(
			(mean - TARGET_MS).abs() < 10.0 && max < TARGET_MS + HEADROOM_MS,
			"mean {mean} max {max}"
		);
		assert!(
			(rs.ratio() - 49_000.0 / 48_000.0).abs() < 0.004,
			"ratio {}",
			rs.ratio()
		);
		let drops_before = rs.stats().overflows;
		let _ = simulate(&mut rs, &[49_000.0], 1.0, 10.0, 4);
		assert_eq!(rs.stats().overflows, drops_before, "settled: no more drops");
		assert!(max_step(&out[48_000 * 2..]) < CLICK);
		// And slow.
		let mut rs = DriftResampler::new();
		let (_, fills) = simulate(&mut rs, &[47_000.0], 1.0, 40.0, 5);
		let late = &fills[fills.len() * 3 / 4..];
		let mean = late.iter().sum::<f64>() / late.len() as f64;
		assert!((mean - TARGET_MS).abs() < 10.0, "mean {mean}");
	}

	#[test]
	fn rate_wandering_46_8k_to_56_4k_stays_bounded_and_click_free() {
		// The measured range, 200 ms segments, mean about 48k (the sink keeps time on average).
		let rates = [
			46_800.0, 56_400.0, 46_800.0, 47_000.0, 41_000.0, 50_000.0, 48_000.0,
		];
		let mean_rate = rates.iter().sum::<f64>() / rates.len() as f64;
		let mut rs = DriftResampler::new();
		let (out, fills) = simulate(&mut rs, &rates, 0.2, 60.0, 11);
		let early = rs.stats();
		let late = &fills[fills.len() / 2..];
		let max = late.iter().cloned().fold(0.0, f64::max);
		let mean = late.iter().sum::<f64>() / late.len() as f64;
		assert!(
			max <= TARGET_MAX_MS + HEADROOM_MS + 6.0,
			"latency bounded: max {max} ms"
		);
		assert!(
			mean < TARGET_MAX_MS,
			"mean {mean} ms (source mean {mean_rate})"
		);
		assert!(max_step(&out) < CLICK, "click {}", max_step(&out));
		// The target adapted: the next minute is (nearly) dropout-free.
		let (out, fills) = simulate(&mut rs, &rates, 0.2, 60.0, 12);
		let s = rs.stats();
		assert!(
			s.underruns - early.underruns <= 3,
			"{early:?} -> {s:?}, target {} ms",
			rs.target_ms()
		);
		assert!(s.overflows - early.overflows <= 3, "{early:?} -> {s:?}");
		// (The second run's tone restarts at phase 0: skip that seam, 200 ms in.)
		assert!(max_step(&out[9600 * 2..]) < CLICK);
		let mean = fills.iter().sum::<f64>() / fills.len() as f64;
		assert!(mean < TARGET_MAX_MS, "mean {mean} ms");
		// The ratio never left its limits.
		assert!((rs.ratio() - 1.0).abs() <= (1.0 + BASE_MAX) * (1.0 + P_MAX) - 1.0 + 1e-9);
	}

	#[test]
	fn idle_source_gives_silence_and_restarts_with_a_fade() {
		let mut rs = DriftResampler::new();
		let mut out = Vec::new();
		rs.pull(240, &mut out);
		assert!(out.iter().all(|&s| s == 0.0), "nothing buffered: silence");
		// 30 ms of a full-scale DC arrives: primed, faded in (no step from 0 to 1).
		rs.push(&vec![1.0; 1440 * 2 + 480]);
		out.clear();
		rs.pull(480, &mut out);
		assert!(rs.primed());
		assert!(
			max_step(&out) <= 1.0 / FADE_IN as f32 + 1e-6,
			"fade in {}",
			max_step(&out)
		);
		// Run dry: fades out, then silence.
		out.clear();
		rs.pull(48_00, &mut out);
		assert!(
			max_step(&out) <= 1.0 / FADE_OUT as f32 + 1e-6,
			"fade out {}",
			max_step(&out)
		);
		assert_eq!(*out.last().unwrap(), 0.0);
		assert!(rs.stats().underruns >= 1);
	}
}
