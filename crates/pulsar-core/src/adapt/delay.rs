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
//! Frames are first merged into **arrival groups** (WebRTC's `InterArrival`): a cellular
//! radio holds frames and hands them over in 20–60 ms batches, so per-frame deltas swing
//! between "one frame very late" and "several frames at once" with no queue anywhere. A
//! frame that lands within `BURST_DELTA_MS` of the group and earlier than its send spacing
//! predicts belongs to that burst; deltas are taken group to group (send: last frame of
//! each group, arrival: the group's completion), which cancels the batching. On top of
//! that the smoother's input is clamped around the smoothed value (one 150 ms spike moves
//! it by a few ms) and the slope is capped, so a single late batch can't read as a queue —
//! and Overuse still needs ≥ 2 groups over the threshold for `OVERUSE_TIME_MS`.
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
/// Frames sent within this span of a group's first frame are one group (WebRTC
/// `kTimestampGroupLengthMs`).
const GROUP_LEN_MS: f64 = 5.0;
/// A frame arriving at most this long after the group's last one, and earlier than the
/// send spacing predicts, is part of the same delivery burst (`kBurstDeltaThresholdMs`).
const BURST_DELTA_MS: f64 = 5.0;
/// A burst never spans more than this (`kMaxBurstDurationMs`) — past it a long run of
/// back-to-back frames is a draining queue, which must stay visible.
const MAX_BURST_MS: f64 = 100.0;
/// Arrival spacing off from the send spacing by more than this = a clock jump or a stall,
/// not a queue (`kArrivalTimeOffsetThresholdMs`): re-anchor without a sample.
const ARRIVAL_OFFSET_RESET_MS: f64 = 3000.0;
/// The smoother's input is held within this of the smoothed value, so a lone 150 ms jitter
/// spike moves the smoothed delay by ≤ 3 ms instead of 15. A real queue (a few ms per group)
/// opens its lag behind the smoother gradually — 5 ms/group needs ~10 groups to pass this,
/// and the detector fires within that ramp — and past it the clamp widens (see `update`),
/// so a delay that keeps running away reaches the smoother. Symmetric, so unbiased on symmetric
/// noise, and the accumulated delay itself is never clamped (it stays the true relative
/// delay). Simulated cellular jitter: 30 here vs 50 roughly halves the false Overuse
/// windows at no cost on a clean path.
const SMOOTHER_STEP_CAP_MS: f64 = 30.0;
/// The clamp widens up to 2^this ×.
const CLAMP_WIDEN_MAX: i32 = 6;
/// Slope cap (ms of delay per ms). A batch fitted over clustered arrival times produced
/// slopes of several ms/ms (logged trends of ±1600). Capped so the gain-scaled trend never
/// exceeds `MAX_THRESHOLD_MS`: the threshold only follows trends it has seen, so it stays
/// below the cap and a capped extreme overuse still reads over it.
const TREND_CAP: f64 = MAX_THRESHOLD_MS / (DELTAS_CAP * THRESHOLD_GAIN);

/// Wrap-aware difference of two 90 kHz timestamps in ms (the host clock is 32-bit at
/// 90 kHz ≈ 13 h); negative when `a` is older than `b`.
fn ts_delta_ms(a: u32, b: u32) -> f64 {
	(a.wrapping_sub(b) as i32) as f64 / 90.0
}

/// Frames merged into one inter-arrival group.
#[derive(Clone, Copy, Debug)]
struct Group {
	first_ts: u32,
	last_ts: u32,
	first_arrival_ms: f64,
	/// Arrival of the group's last frame.
	complete_ms: f64,
}

impl Group {
	fn new(ts: u32, arrival_ms: f64) -> Self {
		Self { first_ts: ts, last_ts: ts, first_arrival_ms: arrival_ms, complete_ms: arrival_ms }
	}

	/// Does a (newer) frame extend this group rather than open the next one?
	fn takes(&self, ts: u32, arrival_ms: f64) -> bool {
		if ts_delta_ms(ts, self.first_ts) <= GROUP_LEN_MS {
			return true;
		}
		let arrival_delta = arrival_ms - self.complete_ms;
		let send_delta = ts_delta_ms(ts, self.last_ts);
		arrival_delta - send_delta < 0.0
			&& arrival_delta <= BURST_DELTA_MS
			&& arrival_ms - self.first_arrival_ms < MAX_BURST_MS
	}
}

#[derive(Clone, Debug)]
pub struct Trendline {
	first_arrival_ms: Option<f64>,
	/// The group still collecting frames.
	group: Option<Group>,
	/// The last completed group: the reference for the next delta.
	prev_group: Option<Group>,
	accumulated_ms: f64,
	smoothed_ms: f64,
	/// Consecutive groups the smoother's input was clamped: > 0 above, < 0 below.
	clamp_run: i32,
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
			group: None,
			prev_group: None,
			accumulated_ms: 0.0,
			smoothed_ms: 0.0,
			clamp_run: 0,
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
	///
	/// The frame that closes an arrival group yields one delta (the just-closed group against
	/// the one before it), so the estimate trails by one group (one frame on a smooth path).
	pub fn on_frame(&mut self, send_ts_90k: u32, arrival_ms: f64) -> DelayState {
		self.last_arrival_ms = arrival_ms;
		let Some(mut g) = self.group else {
			self.group = Some(Group::new(send_ts_90k, arrival_ms));
			self.first_arrival_ms.get_or_insert(arrival_ms);
			return self.state;
		};
		if ts_delta_ms(send_ts_90k, g.last_ts) <= 0.0 || arrival_ms < g.complete_ms {
			// An OLDER / duplicate frame (a NACK-retransmitted or reordered one completing
			// late): not a usable delta, and it must NOT become the reference either. Moving
			// `prev` back to it made the next in-order frame look ~a frame early (-(frame time +
			// lateness)) with nothing ever balancing it, so on a link with steady retransmits
			// the accumulated delay slid down forever: a permanent false "Underuse" that kept
			// the controller from ever climbing (seen 2026-11-08: stuck at 1080p30 ~1.6 Mbit).
			// Nor may it extend the current group: its late arrival would fake a delay.
			return self.state;
		}
		if g.takes(send_ts_90k, arrival_ms) {
			g.last_ts = send_ts_90k;
			g.complete_ms = arrival_ms;
			self.group = Some(g);
			return self.state;
		}
		// `g` is complete; this frame opens the next group.
		self.group = Some(Group::new(send_ts_90k, arrival_ms));
		let Some(prev) = self.prev_group.replace(g) else {
			return self.state;
		};
		let d_send_ms = ts_delta_ms(g.last_ts, prev.last_ts);
		let d_arrival_ms = g.complete_ms - prev.complete_ms;
		if d_send_ms > 5_000.0 || (d_arrival_ms - d_send_ms).abs() > ARRIVAL_OFFSET_RESET_MS {
			// A clock jump / long pause: re-anchor (`g` is the new reference) without a sample.
			return self.state;
		}
		self.update(d_send_ms, d_arrival_ms, g.complete_ms);
		self.state
	}

	/// One group-to-group delta into the trendline and the detector.
	fn update(&mut self, d_send_ms: f64, d_arrival_ms: f64, arrival_ms: f64) {
		let delta = d_arrival_ms - d_send_ms;
		self.num_deltas += 1;
		self.accumulated_ms += delta;
		// The outlier clamp widens ×2 per consecutive group clamped on the same side: a lone
		// spike is clamped once, but a delay that keeps running away (a pipe delivering a
		// fraction of the send rate) is no outlier and must reach the smoother — with a fixed
		// cap the smoothed delay rose ≤ 3 ms per group while the arrival spacing stretched,
		// and the slope of an extreme queue fell under the threshold.
		let widen = |run: i32| f64::from(1u32 << run.clamp(0, CLAMP_WIDEN_MAX));
		let hi = self.smoothed_ms + SMOOTHER_STEP_CAP_MS * widen(self.clamp_run);
		let lo = self.smoothed_ms - SMOOTHER_STEP_CAP_MS * widen(-self.clamp_run);
		self.clamp_run = if self.accumulated_ms > hi {
			self.clamp_run.max(0) + 1
		} else if self.accumulated_ms < lo {
			self.clamp_run.min(0) - 1
		} else {
			0
		};
		let input = self.accumulated_ms.clamp(lo, hi);
		self.smoothed_ms = SMOOTHING * self.smoothed_ms + (1.0 - SMOOTHING) * input;
		let x = arrival_ms - self.first_arrival_ms.unwrap_or(arrival_ms);
		self.samples.push_back((x, self.smoothed_ms));
		while self.samples.len() > WINDOW {
			self.samples.pop_front();
		}
		if self.samples.len() == WINDOW {
			if let Some(slope) = linear_fit_slope(&self.samples) {
				self.trend = slope.clamp(-TREND_CAP, TREND_CAP);
			}
		}
		self.detect(d_send_ms, arrival_ms);
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

	/// Steady NACK repair, packet by packet the way the client's hold loop samples frames (a
	/// new RTP timestamp closes the previous frame): every 4th frame loses one packet whose
	/// retransmit lands ~35 ms later, in the middle of the NEXT frame's packets. The path has
	/// no queue — this must not read as a draining one (it did: a permanent false Underuse).
	#[test]
	fn retransmitted_frames_out_of_order_do_not_fake_underuse() {
		let mut t = Trendline::new();
		let frame_ms = 1000.0 / 30.0;
		// (ts, arrival) packets in arrival order.
		let mut pkts: Vec<(u32, f64)> = Vec::new();
		for f in 0..400u32 {
			let ts = (f as f64 * frame_ms * 90.0) as u32;
			let base = f as f64 * frame_ms + 20.0;
			for k in 0..4 {
				if f % 4 == 1 && k == 3 {
					continue; // lost; retransmitted below
				}
				pkts.push((ts, base + k as f64 * 2.0));
			}
			if f % 4 == 1 {
				pkts.push((ts, base + 35.0)); // the retransmit
			}
		}
		pkts.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
		// hold.rs: sample the previous frame whenever the timestamp changes.
		let (mut cur, mut last) = (None::<u32>, 0.0);
		let mut st = DelayState::Normal;
		for (ts, at) in pkts {
			if cur != Some(ts) {
				if let Some(prev) = cur {
					st = t.on_frame(prev, last);
				}
				cur = Some(ts);
			}
			last = at;
		}
		assert_ne!(st, DelayState::Underuse, "trend={}", t.trend());
		assert!(t.trend().abs() < t.threshold_ms(), "trend={} thr={}", t.trend(), t.threshold_ms());
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

	/// Deterministic xorshift64 PRNG, uniform in [0, 1).
	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> f64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			(self.0 >> 11) as f64 / (1u64 << 53) as f64
		}
		fn range(&mut self, lo: f64, hi: f64) -> f64 {
			lo + (hi - lo) * self.next()
		}
	}

	/// A cellular-like path: frames at `fps` see 40 ms base delay plus uniform
	/// `0..jitter_max` ms of random scheduling delay each, plus `growth_ms` of queue per
	/// frame; with `fifo` the radio delivers in order (a late frame holds the ones behind
	/// it), otherwise frames can overtake. Delivery only happens at batch instants 20-60 ms
	/// apart (random), 0.3 ms between the frames of one batch. Returns frames the way the
	/// hold loop reports them: in arrival order, older timestamps dropped.
	fn cellular(rng: &mut Rng, fps: f64, frames: usize, jitter_max: f64, queue_ms: impl Fn(usize) -> f64, fifo: bool, ts0: u32) -> Vec<(u32, f64)> {
		let frame_ms = 1000.0 / fps;
		let mut ready = Vec::with_capacity(frames);
		let mut last = 0.0f64;
		for i in 0..frames {
			let mut r = i as f64 * frame_ms + 40.0 + rng.range(0.0, jitter_max) + queue_ms(i);
			if fifo {
				r = r.max(last);
				last = r;
			}
			ready.push((ts0.wrapping_add((i as f64 * frame_ms * 90.0) as u32), r));
		}
		ready.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
		let mut out = Vec::with_capacity(frames);
		let (mut batch, mut k, mut newest) = (0.0f64, 0u32, None::<u32>);
		for (ts, r) in ready {
			if r > batch {
				while batch < r {
					batch += rng.range(20.0, 60.0);
				}
				k = 0;
			}
			let at = batch + k as f64 * 0.3;
			k += 1;
			if newest.is_none_or(|n| (ts.wrapping_sub(n) as i32) > 0) {
				newest = Some(ts);
				out.push((ts, at));
			}
		}
		out
	}

	/// Per 2 s window (by arrival time): how many windows, in how many the detector said
	/// Overuse at least once, in how many its final state (what the controller samples at
	/// its tick) was Overuse, and the longest Overuse run (ms) — the controller's fast reflex
	/// fires on a run of `FAST_OVERUSE_MS` (500 ms).
	fn overuse_windows(t: &mut Trendline, frames: &[(u32, f64)]) -> (u32, u32, u32, f64) {
		let (mut windows, mut any, mut at_end, mut longest) = (0u32, 0u32, 0u32, 0.0f64);
		let mut end = frames.first().map_or(0.0, |f| f.1) + 2000.0;
		let (mut seen, mut st) = (false, DelayState::Normal);
		for &(ts, at) in frames {
			if at >= end {
				windows += 1;
				any += seen as u32;
				at_end += (st == DelayState::Overuse) as u32;
				seen = false;
				end += 2000.0;
			}
			st = t.on_frame(ts, at);
			seen |= st == DelayState::Overuse;
			longest = longest.max(t.overuse_ms(at));
		}
		(windows, any, at_end, longest)
	}

	/// Mobile tethering: no queue, but the radio hands frames over in 20-60 ms batches and
	/// each frame sees 0-150 ms of random scheduling delay (in order, or overtaking). Per
	/// frame this read as Overuse in nearly every window at 60/120 fps (simulated before the
	/// grouping: 67-100 % of windows, 10-39 % at the tick) — a cut every window on a link
	/// with nothing queued. Starts near the 32-bit timestamp wrap.
	#[test]
	fn cellular_batches_and_jitter_on_a_flat_path_rarely_read_as_overuse() {
		for &fps in &[30.0, 60.0, 120.0] {
			for &fifo in &[true, false] {
				let (mut windows, mut any, mut at_end, mut longest) = (0, 0, 0, 0.0f64);
				for seed in 1..=8u64 {
					let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
					let ts0 = u32::MAX - 90 * 30_000;
					let fr = cellular(&mut rng, fps, (fps * 120.0) as usize, 150.0, |_| 0.0, fifo, ts0);
					let mut t = Trendline::new();
					let (w, a, e, l) = overuse_windows(&mut t, &fr);
					(windows, any, at_end, longest) = (windows + w, any + a, at_end + e, longest.max(l));
				}
				let tag = format!("fps={fps} fifo={fifo}: {any}/{at_end} of {windows} windows, longest {longest:.0} ms");
				assert!(windows > 400, "{tag}");
				assert!(any as f64 <= 0.05 * windows as f64, "Overuse in > 5 % of windows: {tag}");
				assert!(at_end as f64 <= 0.02 * windows as f64, "Overuse at > 2 % of ticks: {tag}");
				assert!(longest < 500.0, "a false Overuse long enough for the fast reflex: {tag}");
			}
		}
	}

	/// The same cellular path with a real queue growing under the jitter is still caught.
	#[test]
	fn a_queue_growing_under_cellular_jitter_is_still_detected() {
		for &fps in &[30.0, 60.0, 120.0] {
			for seed in 1..=8u64 {
				let n0 = (fps * 10.0) as usize;
				let start_ms = n0 as f64 * 1000.0 / fps;
				for &(jitter, growth) in &[(0.0, 5.0), (50.0, 2.0), (150.0, 5.0)] {
					let mut rng = Rng(0x1234 ^ seed);
					let fr = cellular(&mut rng, fps, n0 * 2, jitter, |i| i.saturating_sub(n0) as f64 * growth, true, 0);
					let mut t = Trendline::new();
					let hit = fr.iter().find_map(|&(ts, at)| {
						let sent = ts as f64 / 90.0;
						(t.on_frame(ts, at) == DelayState::Overuse && sent >= start_ms).then_some(sent - start_ms)
					});
					let hit = hit.unwrap_or_else(|| panic!("never detected: fps={fps} seed={seed} jitter={jitter} growth={growth}"));
					assert!(hit <= 1500.0, "fps={fps} seed={seed} jitter={jitter} growth={growth}: {hit:.0} ms");
				}
			}
		}
	}

	/// A clean path whose queue starts growing 5 ms per frame: Overuse within ~200 ms (one
	/// group later than per-frame sampling; at 30 fps 200 ms is only 6 frames).
	#[test]
	fn a_queue_growing_5ms_per_group_reads_as_overuse_within_200ms() {
		for &(fps, limit_ms) in &[(60.0, 200.0), (120.0, 200.0), (30.0, 450.0)] {
			let mut t = Trendline::new();
			let frame_ms = 1000.0 / fps;
			let n0 = (fps * 5.0) as usize;
			let mut hit = None;
			for i in 0..n0 * 2 {
				let sent = i as f64 * frame_ms;
				let lag = i.saturating_sub(n0) as f64 * 5.0;
				if t.on_frame((sent * 90.0) as u32, sent + 20.0 + lag) == DelayState::Overuse {
					assert!(i >= n0, "false Overuse before the queue at frame {i}");
					hit = Some(sent - n0 as f64 * frame_ms);
					break;
				}
			}
			let hit = hit.expect("never detected");
			assert!(hit <= limit_ms, "fps={fps}: {hit:.0} ms");
		}
	}

	/// One late batch on an otherwise ideal path (six frames held 80 ms, then released
	/// together; and separately one frame 150 ms late) is not a queue.
	#[test]
	fn a_single_late_batch_or_frame_is_not_overuse() {
		let frame_ms = 1000.0 / 60.0;
		for &(first, len, hold_ms) in &[(300usize, 6usize, 80.0), (300, 1, 150.0)] {
			let mut t = Trendline::new();
			let mut last_at = 0.0f64;
			for i in 0..600usize {
				let sent = i as f64 * frame_ms;
				let mut at = sent + 20.0;
				if (first..first + len).contains(&i) {
					// Released together right after the batch's end.
					at = (first as f64 * frame_ms + 20.0 + hold_ms).max(at) + (i - first) as f64 * 0.3;
				}
				at = at.max(last_at); // in order
				last_at = at;
				let st = t.on_frame((sent * 90.0) as u32, at);
				assert_ne!(st, DelayState::Overuse, "frame {i} (batch {first}+{len}, {hold_ms} ms) trend={}", t.trend());
			}
		}
	}

	/// Frames delivered back to back, earlier than their send spacing, form one group (one
	/// delta), not one delta each; a frame later than the send spacing predicts opens a new
	/// group even when it lands within the burst window.
	#[test]
	fn burst_frames_merge_into_one_group() {
		let mut t = Trendline::new();
		let ts = |ms: f64| (ms * 90.0) as u32;
		t.on_frame(ts(0.0), 100.0);
		t.on_frame(ts(33.3), 133.3);
		t.on_frame(ts(66.7), 166.7);
		assert_eq!(t.num_deltas, 1);
		// Three frames held by the radio, then delivered within 1 ms: one group.
		t.on_frame(ts(100.0), 260.0);
		t.on_frame(ts(133.3), 260.4);
		t.on_frame(ts(166.7), 260.8);
		assert_eq!(t.num_deltas, 2, "the held frames closed only the group before them");
		t.on_frame(ts(200.0), 300.0);
		assert_eq!(t.num_deltas, 3, "the burst closed as one group");
		// Group-to-group: send 166.7 - 66.7 = 100, arrival 260.8 - 166.7 = 94.1.
		assert!((t.accumulated_ms - (-5.9)).abs() < 0.2, "{}", t.accumulated_ms);
		// The open group starts at (200, 300). A frame sent 6 ms later (past GROUP_LEN_MS)
		// arriving 8 ms later is later than its send spacing: not a burst member.
		let g = t.group.unwrap();
		assert!(!g.takes(ts(206.0), 304.0 + 4.0), "arrival delta 8 > send delta 6 is not a burst");
		assert!(g.takes(ts(203.0), 301.0), "within the group length");
	}

	/// The capped slope keeps the logged trend bounded (it read ±1600 on batches) and the
	/// outlier clamp lets a runaway delay through: an extreme queue still reads as Overuse,
	/// quickly, at every frame rate.
	#[test]
	fn extreme_queue_is_capped_but_still_overuse() {
		for &fps in &[30.0, 60.0, 120.0] {
			for &growth in &[20.0, 50.0, 200.0] {
				let mut t = Trendline::new();
				feed(&mut t, (fps * 2.0) as usize, fps, 0.0, 0.0);
				// Each frame `growth` ms later than the last: a pipe delivering a fraction of
				// what's sent (50 ms at 60 fps ≈ 1/4).
				let frame_ms = 1000.0 / fps;
				let mut hit = None;
				for i in 0..(fps as usize) {
					let sent = 2000.0 + i as f64 * frame_ms;
					if t.on_frame((sent * 90.0) as u32, sent + 20.0 + i as f64 * growth) == DelayState::Overuse {
						hit = Some(i);
						break;
					}
				}
				let tag = format!("fps={fps} growth={growth} trend={} thr={}", t.trend(), t.threshold_ms());
				assert!(matches!(hit, Some(i) if i <= 12), "{hit:?} {tag}");
				assert!(t.trend() <= MAX_THRESHOLD_MS + 1e-9 && t.trend() > t.threshold_ms(), "{tag}");
			}
		}
	}

	/// A long pause in the stream (send clock jumps 10 s) re-anchors without a sample.
	#[test]
	fn a_long_pause_re_anchors_without_a_sample() {
		let mut t = Trendline::new();
		feed(&mut t, 120, 60.0, 0.0, 0.0);
		let (n, acc) = (t.num_deltas, t.accumulated_ms);
		// Resume 10 s later: the delta across the pause is skipped.
		feed(&mut t, 120, 60.0, 0.0, 12_000.0);
		assert_eq!(t.state(), DelayState::Normal, "trend={}", t.trend());
		assert!((t.accumulated_ms - acc).abs() < 1.0, "pause leaked into the delay: {} vs {acc}", t.accumulated_ms);
		assert_eq!(t.num_deltas, n + 120 - 1, "one delta per closed group, the pause one skipped");
	}
}
