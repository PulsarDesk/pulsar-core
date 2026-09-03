//! One-way delay-gradient estimator (adaptive streaming Phase 3 — the GCC idea, after
//! WebRTC's `TrendlineEstimator` + `OveruseDetector`).
//!
//! A queue building somewhere on the path shows up as the **inter-arrival time of frames
//! growing faster than their inter-send time** — long before packets are dropped and
//! before a keepalive RTT (sampled twice a second) catches it. Per frame the caller passes
//! the host's RTP timestamp (90 kHz, its send clock) and the local arrival time of the
//! frame's last packet; the estimator accumulates the delay variation, smooths it, fits a
//! line through the last 20 samples and compares the slope against an adaptive threshold.
//!
//! Pure: time is passed in (ms on any monotonic clock), no I/O.

/// The detector's current hypothesis about the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DelayState {
	#[default]
	Normal,
	/// Delay keeps growing — a queue is filling. Back off before loss appears.
	Overuse,
	/// Delay shrinking — a queue is draining. Hold (don't climb yet).
	Underuse,
}

const WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const INITIAL_THRESHOLD_MS: f64 = 12.5;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const OVERUSE_TIME_MS: f64 = 10.0;
const MAX_ADAPT_OFFSET_MS: f64 = 15.0;
const DELTAS_CAP: f64 = 60.0;
const MIN_THRESHOLD_MS: f64 = 6.0;
const MAX_THRESHOLD_MS: f64 = 600.0;

#[derive(Clone, Debug)]
pub struct Trendline {
	first_arrival_ms: Option<f64>,
	prev_send_ts: Option<u32>,
	prev_arrival_ms: f64,
	accumulated_ms: f64,
	smoothed_ms: f64,
	samples: std::collections::VecDeque<(f64, f64)>,
	num_deltas: u32,
	trend: f64,
	prev_trend: f64,
	threshold_ms: f64,
	last_threshold_update_ms: Option<f64>,
	time_over_using_ms: f64,
	overuse_counter: u32,
	state: DelayState,
	/// When the current Overuse hypothesis started (ms), for the fast-path reflex.
	overuse_since_ms: Option<f64>,
	last_arrival_ms: f64,
}

impl Default for Trendline {
	fn default() -> Self {
		Self::new()
	}
}

impl Trendline {
	pub fn new() -> Self {
		Self {
			first_arrival_ms: None,
			prev_send_ts: None,
			prev_arrival_ms: 0.0,
			accumulated_ms: 0.0,
			smoothed_ms: 0.0,
			samples: std::collections::VecDeque::with_capacity(WINDOW + 1),
			num_deltas: 0,
			trend: 0.0,
			prev_trend: 0.0,
			threshold_ms: INITIAL_THRESHOLD_MS,
			last_threshold_update_ms: None,
			time_over_using_ms: -1.0,
			overuse_counter: 0,
			state: DelayState::Normal,
			overuse_since_ms: None,
			last_arrival_ms: 0.0,
		}
	}

	/// Forget everything (a stream restart re-bases the host clock).
	pub fn reset(&mut self) {
		*self = Self::new();
	}

	pub fn state(&self) -> DelayState {
		self.state
	}

	/// The gain-scaled slope (ms of delay growth per ms, ×gain) — for logs.
	pub fn trend(&self) -> f64 {
		self.trend * self.num_deltas.min(DELTAS_CAP as u32) as f64 * THRESHOLD_GAIN
	}

	pub fn threshold_ms(&self) -> f64 {
		self.threshold_ms
	}

	/// How long the Overuse hypothesis has held (ms), 0 when not overusing.
	pub fn overuse_ms(&self, now_ms: f64) -> f64 {
		match (self.state, self.overuse_since_ms) {
			(DelayState::Overuse, Some(t)) => (now_ms - t).max(0.0),
			_ => 0.0,
		}
	}

	/// One frame: its RTP timestamp (90 kHz, wraps) and the local arrival time of its last
	/// packet (ms). Returns the updated hypothesis.
	pub fn on_frame(&mut self, send_ts_90k: u32, arrival_ms: f64) -> DelayState {
		self.last_arrival_ms = arrival_ms;
		let Some(prev_ts) = self.prev_send_ts else {
			self.prev_send_ts = Some(send_ts_90k);
			self.prev_arrival_ms = arrival_ms;
			self.first_arrival_ms = Some(arrival_ms);
			return self.state;
		};
		// Wrap-aware send delta (the host clock is 32-bit at 90 kHz ≈ 13 h).
		let d_send_ms = (send_ts_90k.wrapping_sub(prev_ts) as i32) as f64 / 90.0;
		let d_arrival_ms = arrival_ms - self.prev_arrival_ms;
		self.prev_send_ts = Some(send_ts_90k);
		self.prev_arrival_ms = arrival_ms;
		if d_send_ms <= 0.0 || d_send_ms > 5_000.0 || d_arrival_ms < 0.0 {
			// Reordered / duplicate frame or a clock jump: not a usable delta.
			return self.state;
		}
		let delta = d_arrival_ms - d_send_ms;
		self.num_deltas += 1;
		self.accumulated_ms += delta;
		self.smoothed_ms = SMOOTHING * self.smoothed_ms + (1.0 - SMOOTHING) * self.accumulated_ms;
		let x = arrival_ms - self.first_arrival_ms.unwrap_or(arrival_ms);
		self.samples.push_back((x, self.smoothed_ms));
		while self.samples.len() > WINDOW {
			self.samples.pop_front();
		}
		if self.samples.len() == WINDOW {
			if let Some(slope) = linear_fit_slope(&self.samples) {
				self.trend = slope;
			}
		}
		self.detect(d_send_ms, arrival_ms);
		self.state
	}

	fn detect(&mut self, ts_delta_ms: f64, now_ms: f64) {
		if self.num_deltas < 2 {
			self.state = DelayState::Normal;
			return;
		}
		let modified = self.trend();
		let mut next = self.state;
		if modified > self.threshold_ms {
			if self.time_over_using_ms < 0.0 {
				self.time_over_using_ms = ts_delta_ms / 2.0;
			} else {
				self.time_over_using_ms += ts_delta_ms;
			}
			self.overuse_counter += 1;
			if self.time_over_using_ms > OVERUSE_TIME_MS
				&& self.overuse_counter > 1
				&& self.trend >= self.prev_trend
			{
				self.time_over_using_ms = 0.0;
				self.overuse_counter = 0;
				next = DelayState::Overuse;
			}
		} else if modified < -self.threshold_ms {
			self.time_over_using_ms = -1.0;
			self.overuse_counter = 0;
			next = DelayState::Underuse;
		} else {
			self.time_over_using_ms = -1.0;
			self.overuse_counter = 0;
			next = DelayState::Normal;
		}
		self.prev_trend = self.trend;
		if next == DelayState::Overuse {
			if self.state != DelayState::Overuse {
				self.overuse_since_ms = Some(now_ms);
			}
		} else {
			self.overuse_since_ms = None;
		}
		self.state = next;
		self.update_threshold(modified, now_ms);
	}

	fn update_threshold(&mut self, modified_trend: f64, now_ms: f64) {
		let Some(last) = self.last_threshold_update_ms else {
			self.last_threshold_update_ms = Some(now_ms);
			return;
		};
		if modified_trend.abs() > self.threshold_ms + MAX_ADAPT_OFFSET_MS {
			// A big spike: don't let it drag the threshold up.
			self.last_threshold_update_ms = Some(now_ms);
			return;
		}
		let k = if modified_trend.abs() < self.threshold_ms { K_DOWN } else { K_UP };
		let dt = (now_ms - last).clamp(0.0, 100.0);
		self.threshold_ms += k * (modified_trend.abs() - self.threshold_ms) * dt;
		self.threshold_ms = self.threshold_ms.clamp(MIN_THRESHOLD_MS, MAX_THRESHOLD_MS);
		self.last_threshold_update_ms = Some(now_ms);
	}
}

/// Least-squares slope of `y` over `x`; `None` when `x` has no spread.
fn linear_fit_slope(samples: &std::collections::VecDeque<(f64, f64)>) -> Option<f64> {
	let n = samples.len() as f64;
	let (sx, sy) = samples.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
	let (mx, my) = (sx / n, sy / n);
	let (mut num, mut den) = (0.0, 0.0);
	for &(x, y) in samples {
		num += (x - mx) * (y - my);
		den += (x - mx) * (x - mx);
	}
	(den > 0.0).then(|| num / den)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Feed `n` frames at `fps` with per-frame arrival lag growing by `growth_ms` per frame
	/// (0 = an ideal path). Returns the final state.
	fn feed(t: &mut Trendline, n: usize, fps: f64, growth_ms: f64, start_ms: f64) -> DelayState {
		let frame_ms = 1000.0 / fps;
		let mut st = DelayState::Normal;
		for i in 0..n {
			let ts = ((start_ms + i as f64 * frame_ms) * 90.0) as u32;
			let arrival = start_ms + i as f64 * frame_ms + 20.0 + i as f64 * growth_ms;
			st = t.on_frame(ts, arrival);
		}
		st
	}

	#[test]
	fn ideal_path_stays_normal() {
		let mut t = Trendline::new();
		assert_eq!(feed(&mut t, 200, 60.0, 0.0, 0.0), DelayState::Normal);
		assert!(t.trend().abs() < 1.0, "{}", t.trend());
	}

	#[test]
	fn growing_queue_is_detected_as_overuse_before_any_loss() {
		let mut t = Trendline::new();
		feed(&mut t, 60, 60.0, 0.0, 0.0);
		// From now on every frame arrives 1 ms later than the previous one (queue filling
		// at 60 ms per second — a 2 Mbit pipe fed 2.1 Mbit).
		let st = feed(&mut t, 60, 60.0, 1.0, 1000.0);
		assert_eq!(st, DelayState::Overuse, "trend={} thr={}", t.trend(), t.threshold_ms());
		assert!(t.overuse_ms(2000.0) > 0.0);
	}

	#[test]
	fn draining_queue_is_underuse_then_normal() {
		let mut t = Trendline::new();
		feed(&mut t, 60, 60.0, 1.0, 0.0);
		// Now frames arrive progressively earlier (queue draining).
		let mut st = DelayState::Normal;
		for i in 0..40 {
			let base = 1000.0 + i as f64 * 16.667;
			let ts = (base * 90.0) as u32;
			st = t.on_frame(ts, base + 80.0 - i as f64 * 1.5);
		}
		assert_eq!(st, DelayState::Underuse, "trend={}", t.trend());
		assert_eq!(feed(&mut t, 120, 60.0, 0.0, 2000.0), DelayState::Normal);
	}

	#[test]
	fn timestamp_wraparound_and_reorders_do_not_poison_the_estimate() {
		let mut t = Trendline::new();
		// Start just below the 32-bit wrap.
		let start_ts: u32 = u32::MAX - 90 * 100;
		for i in 0..80u32 {
			let ts = start_ts.wrapping_add(i * 1500); // 60 fps in 90 kHz ticks
			t.on_frame(ts, i as f64 * 16.667 + 10.0);
		}
		assert_eq!(t.state(), DelayState::Normal, "trend={}", t.trend());
		// A duplicated / reordered frame is ignored, not counted as a huge delta.
		let before = t.trend();
		t.on_frame(start_ts, 2000.0);
		assert_eq!(t.trend(), before);
	}

	#[test]
	fn threshold_adapts_within_bounds() {
		let mut t = Trendline::new();
		feed(&mut t, 400, 60.0, 0.05, 0.0);
		let th = t.threshold_ms();
		assert!((MIN_THRESHOLD_MS..=MAX_THRESHOLD_MS).contains(&th), "{th}");
	}
}
