//! Acoustic echo canceller for voice calls (`crate::call`): removes the far end (the peer's
//! voice we play) from what we send back — the microphone (speaker → room → mic) and, on a
//! Linux host, the system-audio capture (which hears everything the host plays). Pure Rust, no
//! dependencies, for every target.
//!
//! Partitioned-block frequency-domain adaptive filter (the MDF/PBFDAF family Speex and WebRTC
//! AEC build on), 48 kHz mono:
//! - blocks of [`BLOCK`] samples, an FFT of twice that (overlap-save), [`PARTITIONS`] filter
//!   partitions → an echo tail of `BLOCK × PARTITIONS` samples (~213 ms: the playback queue,
//!   device buffers and the room);
//! - NLMS update normalised per frequency bin by the far end's power; the gradient constraint
//!   is applied to one partition per block, round-robin (as Speex does: cheap, still
//!   converges);
//! - double-talk without a detector: two filters (two-path, as in Speex / WebRTC). The
//!   background filter adapts on every block; the foreground filter, which makes the output,
//!   takes the background's taps only once the background cancels clearly better. Near-end
//!   speech makes the background diverge for a moment — the foreground keeps the good taps,
//!   and the background is put back to them once it is clearly worse;
//! - residual echo suppression: while the output is mostly echo residue (the foreground
//!   removes well over 10 dB and nobody talks here), it is attenuated by [`RESIDUAL_GAIN`]
//!   (smoothed); never while the near end talks.
//!
//! The caller aligns the far end so that its echo arrives LATER in the near signal (never
//! earlier): delays from 0 to the tail are learned.

use std::f32::consts::PI;

/// Samples per block (5.3 ms at 48 kHz).
pub const BLOCK: usize = 256;
/// Filter partitions: the echo tail is `BLOCK × PARTITIONS` samples.
pub const PARTITIONS: usize = 40;
/// The echo tail in samples.
pub const TAIL: usize = BLOCK * PARTITIONS;
const N2: usize = BLOCK * 2;
/// NLMS step size.
const MU: f32 = 0.5;
/// Far-end power under which the far end counts as silent (≈ −60 dBFS mean square).
const FAR_SILENT: f32 = 1e-6;
/// Background error this far under the foreground's (smoothed power) → copy it over.
const COPY_BELOW: f32 = 0.6;
/// Background error this far over the foreground's → it diverged (double-talk): reset it.
const RESET_ABOVE: f32 = 4.0;
/// Power smoothing per block for those comparisons.
const SMOOTH: f32 = 0.3;
/// The foreground removing more than this (10 dB) = the output is echo residue.
const ECHO_ONLY: f32 = 0.1;
/// Output gain while only the far end talks (residual echo suppression, −12 dB).
const RESIDUAL_GAIN: f32 = 0.25;

#[derive(Clone, Copy, Default, Debug)]
struct C {
	re: f32,
	im: f32,
}

impl C {
	fn mul(self, o: C) -> C {
		C { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
	}
	fn conj(self) -> C {
		C { re: self.re, im: -self.im }
	}
	fn norm2(self) -> f32 {
		self.re * self.re + self.im * self.im
	}
}

/// In-place iterative radix-2 complex FFT of size [`N2`].
struct Fft {
	tw: Vec<C>,
	rev: Vec<usize>,
}

impl Fft {
	fn new() -> Fft {
		let n = N2;
		let bits = n.trailing_zeros();
		let rev = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
		let tw = (0..n / 2)
			.map(|k| {
				let a = -2.0 * PI * k as f32 / n as f32;
				C { re: a.cos(), im: a.sin() }
			})
			.collect();
		Fft { tw, rev }
	}

	/// Forward (`inverse` = false) or inverse (unscaled) transform.
	fn run(&self, a: &mut [C], inverse: bool) {
		let n = a.len();
		for i in 0..n {
			let j = self.rev[i];
			if i < j {
				a.swap(i, j);
			}
		}
		let mut len = 2;
		while len <= n {
			let step = n / len;
			for start in (0..n).step_by(len) {
				for k in 0..len / 2 {
					let mut w = self.tw[k * step];
					if inverse {
						w = w.conj();
					}
					let u = a[start + k];
					let v = a[start + k + len / 2].mul(w);
					a[start + k] = C { re: u.re + v.re, im: u.im + v.im };
					a[start + k + len / 2] = C { re: u.re - v.re, im: u.im - v.im };
				}
			}
			len <<= 1;
		}
	}

	fn forward_real(&self, x: &[f32], out: &mut [C]) {
		for (o, &v) in out.iter_mut().zip(x) {
			*o = C { re: v, im: 0.0 };
		}
		self.run(out, false);
	}

	/// Inverse, scaled by 1/N, real parts into `out`.
	fn inverse_real(&self, spec: &mut [C], out: &mut [f32]) {
		self.run(spec, true);
		let s = 1.0 / spec.len() as f32;
		for (o, c) in out.iter_mut().zip(spec.iter()) {
			*o = c.re * s;
		}
	}
}

/// One echo canceller (one near-end channel against one far end).
pub struct Aec {
	fft: Fft,
	/// The previous far-end block (overlap-save input is [previous, current]).
	x_prev: Vec<f32>,
	/// Far-end spectra, newest at `head`, ring of `PARTITIONS`.
	x_hist: Vec<Vec<C>>,
	head: usize,
	/// Background filter partitions (frequency domain, aligned with `x_hist` by age): adapts.
	w: Vec<Vec<C>>,
	/// Foreground filter: makes the output.
	fg: Vec<Vec<C>>,
	/// Smoothed powers: foreground error, background error, near end.
	pe_fg: f32,
	pe_bg: f32,
	pd: f32,
	/// Mean-square far-end level of the last `PARTITIONS` blocks (the tail).
	far_pow: Vec<f32>,
	next_constrain: usize,
	gain: f32,
	// Scratch.
	spec: Vec<C>,
	tmp: Vec<f32>,
}

impl Default for Aec {
	fn default() -> Self {
		Aec::new()
	}
}

impl Aec {
	pub fn new() -> Aec {
		Aec {
			fft: Fft::new(),
			x_prev: vec![0.0; BLOCK],
			x_hist: vec![vec![C::default(); N2]; PARTITIONS],
			head: 0,
			w: vec![vec![C::default(); N2]; PARTITIONS],
			fg: vec![vec![C::default(); N2]; PARTITIONS],
			pe_fg: 0.0,
			pe_bg: 0.0,
			pd: 0.0,
			far_pow: vec![0.0; PARTITIONS],
			next_constrain: 0,
			gain: 1.0,
			spec: vec![C::default(); N2],
			tmp: vec![0.0; N2],
		}
	}

	/// Whether the far end played anything within the echo tail (when not, `process` passes the
	/// near end through untouched).
	pub fn far_active(&self) -> bool {
		self.far_pow.iter().any(|&p| p > FAR_SILENT)
	}

	/// One block: `far` = what was played (aligned so its echo comes later in `near`), `near` =
	/// what was captured; writes the near end without the echo into `out`. All `BLOCK` long,
	/// samples in −1..1.
	pub fn process(&mut self, far: &[f32], near: &[f32], out: &mut [f32]) {
		assert!(far.len() == BLOCK && near.len() == BLOCK && out.len() == BLOCK);
		// Far-end spectrum of [previous, current] into the ring.
		self.head = (self.head + PARTITIONS - 1) % PARTITIONS;
		self.tmp[..BLOCK].copy_from_slice(&self.x_prev);
		self.tmp[BLOCK..].copy_from_slice(far);
		self.fft.forward_real(&self.tmp, &mut self.x_hist[self.head]);
		self.x_prev.copy_from_slice(far);
		let px = far.iter().map(|v| v * v).sum::<f32>() / BLOCK as f32;
		self.far_pow.rotate_right(1);
		self.far_pow[0] = px;
		if !self.far_active() {
			out.copy_from_slice(near);
			self.gain = 1.0;
			return;
		}

		let mut e_fg = [0f32; BLOCK];
		let mut e_bg = [0f32; BLOCK];
		self.error(true, near, &mut e_fg);
		self.error(false, near, &mut e_bg);
		let pow = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / BLOCK as f32;
		self.pe_fg += (pow(&e_fg) - self.pe_fg) * SMOOTH;
		self.pe_bg += (pow(&e_bg) - self.pe_bg) * SMOOTH;
		self.pd += (pow(near) - self.pd) * SMOOTH;

		if px > FAR_SILENT {
			self.adapt(&e_bg);
		}
		// Two-path: take the background's taps when clearly better, reset it when diverged.
		if self.pe_bg < COPY_BELOW * self.pe_fg {
			self.fg.clone_from(&self.w);
			self.pe_fg = self.pe_bg;
		} else if self.pe_bg > RESET_ABOVE * self.pe_fg && self.pe_bg > FAR_SILENT {
			self.w.clone_from(&self.fg);
			self.pe_bg = self.pe_fg;
		}

		// Residual echo suppression while the output is echo residue only.
		let echo_only = self.pe_fg < ECHO_ONLY * self.pd;
		let target = if echo_only { RESIDUAL_GAIN } else { 1.0 };
		for i in 0..BLOCK {
			self.gain += (target - self.gain) * 0.02;
			out[i] = e_fg[i] * self.gain;
		}
	}

	/// `near` minus the echo the foreground (`fg`) or background filter predicts.
	fn error(&mut self, fg: bool, near: &[f32], err: &mut [f32]) {
		for c in self.spec.iter_mut() {
			*c = C::default();
		}
		let w = if fg { &self.fg } else { &self.w };
		for p in 0..PARTITIONS {
			let x = &self.x_hist[(self.head + p) % PARTITIONS];
			for (s, (w, x)) in self.spec.iter_mut().zip(w[p].iter().zip(x)) {
				let m = w.mul(*x);
				s.re += m.re;
				s.im += m.im;
			}
		}
		self.fft.inverse_real(&mut self.spec, &mut self.tmp);
		for i in 0..BLOCK {
			err[i] = near[i] - self.tmp[BLOCK + i];
		}
	}

	/// NLMS step of the background filter on its error.
	fn adapt(&mut self, err: &[f32]) {
		// E = FFT([0…0, e]); per-bin far power over the partitions.
		self.tmp[..BLOCK].fill(0.0);
		self.tmp[BLOCK..].copy_from_slice(err);
		self.fft.forward_real(&self.tmp, &mut self.spec);
		let delta = N2 as f32 * FAR_SILENT;
		let mut step = vec![0f32; N2];
		for x in &self.x_hist {
			for (n, c) in step.iter_mut().zip(x) {
				*n += c.norm2();
			}
		}
		for n in step.iter_mut() {
			*n = MU / (*n + delta);
		}
		for p in 0..PARTITIONS {
			let x = &self.x_hist[(self.head + p) % PARTITIONS];
			for k in 0..N2 {
				let g = x[k].conj().mul(self.spec[k]);
				self.w[p][k].re += g.re * step[k];
				self.w[p][k].im += g.im * step[k];
			}
		}
		// Gradient constraint (taps past BLOCK are circular-convolution wrap): one partition
		// per block.
		let j = self.next_constrain;
		self.next_constrain = (j + 1) % PARTITIONS;
		let mut wspec = std::mem::take(&mut self.w[j]);
		self.fft.inverse_real(&mut wspec, &mut self.tmp);
		self.tmp[BLOCK..].fill(0.0);
		self.fft.forward_real(&self.tmp, &mut wspec);
		self.w[j] = wspec;
	}
}

/// Runs an [`Aec`] over arbitrary chunk sizes: feeds whole blocks, keeps the remainder (adds at
/// most one block of latency to the near end).
pub struct AecStream {
	aec: Aec,
	far: Vec<f32>,
	near: Vec<f32>,
	out: std::collections::VecDeque<f32>,
}

impl Default for AecStream {
	fn default() -> Self {
		AecStream::new()
	}
}

impl AecStream {
	pub fn new() -> AecStream {
		AecStream {
			aec: Aec::new(),
			far: Vec::new(),
			near: Vec::new(),
			// One block of latency, so every call returns as many samples as it got.
			out: std::iter::repeat_n(0.0, BLOCK).collect(),
		}
	}

	/// `far` and `near` the same length (any); returns that many processed samples.
	pub fn process(&mut self, far: &[f32], near: &[f32]) -> Vec<f32> {
		debug_assert_eq!(far.len(), near.len());
		self.far.extend_from_slice(far);
		self.near.extend_from_slice(near);
		let mut block = [0f32; BLOCK];
		while self.near.len() >= BLOCK {
			self.aec.process(&self.far[..BLOCK], &self.near[..BLOCK], &mut block);
			self.out.extend(block.iter().copied());
			self.far.drain(..BLOCK);
			self.near.drain(..BLOCK);
		}
		let n = near.len().min(self.out.len());
		self.out.drain(..n).collect()
	}

	pub fn far_active(&self) -> bool {
		self.aec.far_active()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Deterministic noise.
	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> f32 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			(self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
		}
	}

	/// Speech-like: coloured noise with a syllable-rate envelope.
	fn talker(seed: u64, secs: f32, rate: f32) -> Vec<f32> {
		let mut r = Rng(seed);
		let n = (secs * 48_000.0) as usize;
		let mut lp = 0.0f32;
		(0..n)
			.map(|i| {
				lp = 0.7 * lp + 0.3 * r.next();
				let env = 0.5 + 0.5 * (2.0 * PI * rate * i as f32 / 48_000.0).sin();
				0.3 * lp * env
			})
			.collect()
	}

	fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
		let mut y = vec![0f32; x.len()];
		for (i, &v) in x.iter().enumerate() {
			if v == 0.0 {
				continue;
			}
			for (j, &c) in h.iter().enumerate() {
				if i + j < y.len() {
					y[i + j] += v * c;
				}
			}
		}
		y
	}

	/// A room: `delay` samples, then a decaying random response, overall gain `g`.
	fn room(delay: usize, len: usize, g: f32, seed: u64) -> Vec<f32> {
		let mut r = Rng(seed);
		let mut h = vec![0f32; delay];
		let mut e = 0.0;
		let taps: Vec<f32> = (0..len).map(|i| r.next() * (-(i as f32) / (len as f32 / 5.0)).exp()).collect();
		for t in &taps {
			e += t * t;
		}
		h.extend(taps.iter().map(|t| t * g / e.sqrt()));
		h
	}

	fn run(aec: &mut Aec, far: &[f32], near: &[f32]) -> Vec<f32> {
		let mut out = vec![0f32; near.len()];
		for b in 0..near.len() / BLOCK {
			let r = b * BLOCK..(b + 1) * BLOCK;
			aec.process(&far[r.clone()], &near[r.clone()], &mut out[r]);
		}
		out
	}

	fn pow(x: &[f32]) -> f32 {
		x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32
	}

	fn db(a: f32, b: f32) -> f32 {
		10.0 * (a / b).log10()
	}

	#[test]
	fn fft_round_trip() {
		let f = Fft::new();
		let x: Vec<f32> = (0..N2).map(|i| (i as f32 * 0.37).sin()).collect();
		let mut s = vec![C::default(); N2];
		f.forward_real(&x, &mut s);
		let mut y = vec![0f32; N2];
		f.inverse_real(&mut s, &mut y);
		for (a, b) in x.iter().zip(&y) {
			assert!((a - b).abs() < 1e-4);
		}
	}

	#[test]
	fn cancels_a_room_echo() {
		let far = talker(1, 8.0, 3.0);
		let echo = convolve(&far, &room(1920, 2400, 0.4, 7));
		let mut r = Rng(9);
		let near: Vec<f32> = echo.iter().map(|e| e + 1e-4 * r.next()).collect();
		let out = run(&mut Aec::new(), &far, &near);
		let last = near.len() - 48_000 * 2..near.len();
		let erle = db(pow(&near[last.clone()]), pow(&out[last]));
		eprintln!("room echo: ERLE {erle:.1} dB");
		assert!(erle > 25.0, "ERLE {erle:.1} dB");
	}

	#[test]
	fn cancels_a_digital_loopback() {
		// Linux host: the system-audio capture hears the played voice itself, unity gain.
		let far = talker(2, 6.0, 4.0);
		let mut h = vec![0f32; 1300];
		h.push(1.0);
		let near = convolve(&far, &h);
		let out = run(&mut Aec::new(), &far, &near);
		let last = near.len() - 48_000..near.len();
		let erle = db(pow(&near[last.clone()]), pow(&out[last]));
		eprintln!("digital loopback: ERLE {erle:.1} dB");
		assert!(erle > 30.0, "ERLE {erle:.1} dB");
	}

	#[test]
	fn keeps_the_near_talker_during_double_talk() {
		let far = talker(3, 10.0, 3.0);
		let echo = convolve(&far, &room(2400, 2400, 0.3, 11));
		let me = talker(4, 10.0, 5.0);
		// The near end starts talking after 6 s of echo only.
		let start = 6 * 48_000;
		let near: Vec<f32> = (0..far.len()).map(|i| echo[i] + if i >= start { me[i] } else { 0.0 }).collect();
		let out = run(&mut Aec::new(), &far, &near);
		// Compare one block late (none here: Aec is block-aligned) over the last 3 s.
		let w = far.len() - 3 * 48_000..far.len();
		let residual: Vec<f32> = w.clone().map(|i| out[i] - me[i]).collect();
		let snr = db(pow(&me[w.clone()]), pow(&residual));
		eprintln!("double-talk: near talker {snr:.1} dB over the residue");
		assert!(snr > 10.0, "near talker kept: {snr:.1} dB over what is left of the echo");
		let echo_left = db(pow(&echo[w.clone()]), pow(&residual));
		assert!(echo_left > 10.0, "echo still removed during double-talk: {echo_left:.1} dB");
	}

	#[test]
	fn passes_through_without_far_end() {
		let near = talker(5, 1.0, 3.0);
		let far = vec![0f32; near.len()];
		let out = run(&mut Aec::new(), &far, &near);
		let n = out.len() / BLOCK * BLOCK;
		assert_eq!(&out[..n], &near[..n]);
	}

	#[test]
	fn stream_keeps_length_with_odd_chunks() {
		let mut s = AecStream::new();
		let mut total = 0;
		for len in [960usize, 100, 700, 960, 1] {
			total += s.process(&vec![0.0; len], &vec![0.1; len]).len();
		}
		assert_eq!(total, 960 + 100 + 700 + 960 + 1);
	}
}

#[cfg(test)]
mod bench {
	use super::*;
	/// Real-time headroom (run with --nocapture; informative, never fails).
	#[test]
	fn realtime_cost() {
		let mut a = Aec::new();
		let far: Vec<f32> = (0..BLOCK).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
		let near: Vec<f32> = far.iter().map(|v| v * 0.3).collect();
		let mut out = vec![0f32; BLOCK];
		let blocks = 48_000 * 2 / BLOCK;
		let t = std::time::Instant::now();
		for _ in 0..blocks {
			a.process(&far, &near, &mut out);
		}
		let audio = blocks as f64 * BLOCK as f64 / 48_000.0;
		eprintln!("aec: {:.1} % of real time", t.elapsed().as_secs_f64() / audio * 100.0);
	}
}
