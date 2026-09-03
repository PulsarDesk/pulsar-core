//! Adaptive streaming — validation matrix (Phase 4), the simulated half.
//!
//! The real matrix (encoders × decoders × transports × `tc netem` profiles) needs hardware
//! and a display; what CAN run on every CI box is the controller against a **path model**:
//! a pipe with a capacity, a base RTT, a bounded queue (bufferbloat) and random loss, fed
//! by the controller's own encoder rate + FEC parity, with per-frame arrival times for the
//! delay-gradient estimator. Each cell asserts the acceptance criteria of the design doc:
//! settles under the capacity, lands on a point that is clean at that budget, does not
//! oscillate, flips to recovery mode within seconds of the first loss, keeps FEC overhead
//! under the ceiling. `cargo test --test adapt_scenarios` — a few hundred ms in total.

use pulsar_core::adapt::{self, fec_policy, Config, Controller, Sample};
use pulsar_core::pipeline::VCodec;
use pulsar_core::service::LossRecovery;

/// A netem-like profile: capacity, one-way base delay, random loss.
#[derive(Clone, Copy, Debug)]
struct Profile {
	name: &'static str,
	capacity_kbps: u32,
	delay_ms: f32,
	loss: f32,
}

const PROFILES: &[Profile] = &[
	Profile { name: "lan-20mbit", capacity_kbps: 20_000, delay_ms: 2.0, loss: 0.0 },
	Profile { name: "5mbit-20ms", capacity_kbps: 5_000, delay_ms: 20.0, loss: 0.0 },
	Profile { name: "2mbit-120ms", capacity_kbps: 2_000, delay_ms: 120.0, loss: 0.0 },
	Profile { name: "1mbit-250ms", capacity_kbps: 1_000, delay_ms: 250.0, loss: 0.0 },
	Profile { name: "20mbit-1pct", capacity_kbps: 20_000, delay_ms: 20.0, loss: 0.01 },
	Profile { name: "5mbit-3pct-120ms", capacity_kbps: 5_000, delay_ms: 120.0, loss: 0.03 },
	Profile { name: "2mbit-3pct-120ms", capacity_kbps: 2_000, delay_ms: 120.0, loss: 0.03 },
	Profile { name: "5mbit-10pct-250ms", capacity_kbps: 5_000, delay_ms: 250.0, loss: 0.10 },
];

/// The pipe: a drop-tail queue of `queue_max_ms` worth of capacity; anything beyond it is
/// lost; whatever sits in it adds delay. Random loss on top.
struct Path {
	p: Profile,
	queue_bits: f64,
	queue_max_bits: f64,
	/// Reed-Solomon parity ratio the host currently sends (0 = off).
	fec_ratio: f32,
	clean_windows: u32,
}

impl Path {
	fn new(p: Profile) -> Self {
		let queue_max_bits = p.capacity_kbps as f64 * 1000.0 * 0.25; // 250 ms of buffering
		Self { p, queue_bits: 0.0, queue_max_bits, fec_ratio: 0.0, clean_windows: 0 }
	}

	/// Run one 2 s window at `send_kbps` (encoder + parity). Returns the sample the
	/// client would measure, and feeds the controller's delay estimator with 120 frames.
	fn window(&mut self, ctl: &mut Controller, send_kbps: u32, t0_ms: f64) -> Sample {
		let cap = self.p.capacity_kbps as f64 * 1000.0;
		let send = send_kbps as f64 * 1000.0;
		let frames = 120;
		let frame_ms = 2000.0 / frames as f64;
		let mut lost_bits = 0.0;
		let mut rtt_samples = Vec::new();
		for i in 0..frames {
			// Per frame: offered bits vs drained bits; the excess queues, the overflow drops.
			let offered = send * frame_ms / 1000.0;
			let drained = cap * frame_ms / 1000.0;
			self.queue_bits += offered - drained;
			if self.queue_bits < 0.0 {
				self.queue_bits = 0.0;
			}
			if self.queue_bits > self.queue_max_bits {
				lost_bits += self.queue_bits - self.queue_max_bits;
				self.queue_bits = self.queue_max_bits;
			}
			let queue_delay_ms = self.queue_bits / cap * 1000.0;
			let t = t0_ms + i as f64 * frame_ms;
			ctl.on_frame((t * 90.0) as u32, t + self.p.delay_ms as f64 + queue_delay_ms);
			if i % 30 == 15 {
				rtt_samples.push(2.0 * self.p.delay_ms + queue_delay_ms as f32);
			}
		}
		let total_bits = send * 2.0;
		let pkts = (total_bits / 8.0 / 1100.0).max(1.0);
		let overflow = if total_bits > 0.0 { lost_bits / total_bits } else { 0.0 };
		let raw_loss = (overflow + self.p.loss as f64).min(0.95);
		// Reed-Solomon parity repairs RANDOM loss up to `m` per frame: nearly all of it when
		// the ratio covers the loss, a shrinking share when the loss exceeds the ratio.
		let repaired = if self.fec_ratio > 0.0 && self.p.loss > 0.0 {
			let cover = if self.p.loss <= self.fec_ratio { 0.95 } else { 0.5 * self.fec_ratio / self.p.loss };
			self.p.loss as f64 * cover as f64
		} else {
			0.0
		};
		let loss = (raw_loss - repaired).max(0.0);
		let lost = (pkts * loss).round() as u32;
		let s = Sample {
			recv: (pkts as u32).saturating_sub(lost),
			lost,
			rtt_ms: rtt_samples,
			fec_ok: (pkts * repaired) as u32,
			..Default::default()
		};
		// The host sizes parity from what the client reports (host-side policy).
		if (loss as f32) < fec_policy::OFF_BELOW {
			self.clean_windows += 1;
		} else {
			self.clean_windows = 0;
		}
		self.fec_ratio = fec_policy::parity_ratio(loss as f32, self.fec_ratio, self.clean_windows);
		ctl.set_fec_overhead(self.fec_ratio);
		s
	}
}

struct Run {
	target_last: u32,
	point_last: String,
	point_changes_late: u32,
	overflow_windows_late: u32,
	recovery_at: Option<u32>,
	max_fec_overhead: f32,
	first_loss_at: Option<u32>,
}

fn run(profile: Profile, codec: VCodec, cap_kbps: u32, windows: u32) -> Run {
	let mut cfg = Config::new(codec, cap_kbps);
	cfg.native = (1920, 1080, 60);
	cfg.ir_capable = true;
	cfg.ladder = true; // the matrix exercises the ladder (off by default in the apps)
	let mut ctl = Controller::new(cfg);
	let mut path = Path::new(profile);
	let mut r = Run {
		target_last: 0,
		point_last: String::new(),
		point_changes_late: 0,
		overflow_windows_late: 0,
		recovery_at: None,
		max_fec_overhead: 0.0,
		first_loss_at: None,
	};
	let settle = windows / 3;
	for w in 0..windows {
		let share = fec_policy::encoder_share_ratio(path.fec_ratio);
		let wire = (ctl.encoder_kbps() as f32 / share) as u32;
		r.max_fec_overhead = r.max_fec_overhead.max(path.fec_ratio);
		let s = path.window(&mut ctl, wire, w as f64 * 2000.0);
		if s.lost > 0 && r.first_loss_at.is_none() {
			r.first_loss_at = Some(w);
		}
		let overflow = wire > profile.capacity_kbps;
		if w >= settle && overflow {
			r.overflow_windows_late += 1;
		}
		let d = ctl.tick(&s);
		if w >= settle && d.point.is_some() {
			r.point_changes_late += 1;
		}
		if d.recovery.is_some() && r.recovery_at.is_none() {
			r.recovery_at = Some(w);
		}
	}
	r.target_last = ctl.target_kbps();
	r.point_last = ctl.point().label();
	let _ = LossRecovery::Normal;
	r
}

fn height_of(label: &str) -> u32 {
	label.split('p').next().unwrap().parse().unwrap()
}

#[test]
fn matrix_settles_under_capacity_on_a_clean_point_without_oscillation() {
	for &profile in PROFILES {
		for codec in [VCodec::H264, VCodec::H265] {
			let cap = 20_000;
			let r = run(profile, codec, cap, 300); // 10 minutes
			let tag = format!("{} {codec:?}", profile.name);
			// Settled under the pipe (at most a couple of probe-induced overshoots in the
			// last ~7 minutes).
			assert!(
				r.overflow_windows_late <= 6,
				"{tag}: overshoot windows after settling = {} (target {})",
				r.overflow_windows_late,
				r.target_last
			);
			// The target is within the pipe and not collapsed way under it.
			let cap_eff = profile.capacity_kbps.min(cap) as f32;
			assert!(
				r.target_last as f32 <= cap_eff * 1.05,
				"{tag}: target {} above capacity {}",
				r.target_last,
				profile.capacity_kbps
			);
			if profile.loss < 0.05 {
				assert!(
					r.target_last as f32 >= cap_eff * 0.40,
					"{tag}: target {} collapsed under capacity {}",
					r.target_last,
					profile.capacity_kbps
				);
			}
			// Operating point: the rung is clean at the settled target.
			let expect_h = match profile.capacity_kbps {
				c if c >= 15_000 => 1080,
				c if c >= 4_000 => 720,
				c if c >= 1_800 => 540,
				_ => 360,
			};
			let h = height_of(&r.point_last);
			assert!(
				h >= expect_h.min(if codec == VCodec::H265 { expect_h } else { expect_h }) && h <= 1080,
				"{tag}: point {} for a {} kbit pipe (target {})",
				r.point_last,
				profile.capacity_kbps,
				r.target_last
			);
			// No oscillation once settled.
			assert!(r.point_changes_late <= 2, "{tag}: {} point changes after settling", r.point_changes_late);
			// Recovery mode flips within a few windows of the first loss — unless FEC alone
			// brought the measured loss under the flip threshold (the 1 % profile).
			if let Some(first) = r.first_loss_at {
				if profile.loss >= 0.03 || profile.loss == 0.0 {
					let at = r.recovery_at.unwrap_or(u32::MAX);
					assert!(at <= first + 2, "{tag}: recovery flipped at {at}, first loss at {first}");
				}
			}
			// FEC never exceeds the overhead ceiling.
			assert!(r.max_fec_overhead <= fec_policy::RS_MAX_RATIO + 1e-6, "{tag}: fec overhead {}", r.max_fec_overhead);
		}
	}
}

#[test]
fn forced_20mbit_reaches_1080p60_within_30s_and_forced_2mbit_a_low_rung_within_10s() {
	// 20 Mbit from a cold start hint of 3 Mbit.
	let mut cfg = Config::new(VCodec::H264, 20_000);
	cfg.native = (1920, 1080, 60);
	cfg.start_kbps = 3000;
	cfg.ladder = true;
	let mut ctl = Controller::new(cfg);
	let mut path = Path::new(PROFILES[0]);
	let mut reached = None;
	for w in 0..15 {
		let kbps = ctl.encoder_kbps();
		let s = path.window(&mut ctl, kbps, w as f64 * 2000.0);
		ctl.tick(&s);
		if ctl.point().label() == "1080p60" {
			reached = Some(w);
			break;
		}
	}
	assert!(reached.is_some(), "1080p60 not reached in 30 s: {}", ctl.point().label());

	// 2 Mbit from the default top: a rung that is clean at ≤ 2 Mbit within 10 s.
	let mut cfg = Config::new(VCodec::H264, 20_000);
	cfg.native = (1920, 1080, 60);
	cfg.ladder = true;
	let mut ctl = Controller::new(cfg);
	let mut path = Path::new(PROFILES[2]);
	for w in 0..5 {
		let kbps = ctl.encoder_kbps();
		let s = path.window(&mut ctl, kbps, w as f64 * 2000.0);
		ctl.tick(&s);
	}
	let h = height_of(&ctl.point().label());
	assert!(h <= 720, "still at {} after 10 s on a 2 Mbit pipe (target {})", ctl.point().label(), ctl.target_kbps());
}

#[test]
fn delay_gradient_backs_off_before_the_queue_overflows() {
	// A 5 Mbit pipe with a big queue (1 s): loss appears only after the queue fills; the
	// trendline must cut on the growing delay before that.
	let mut cfg = Config::new(VCodec::H264, 6000);
	cfg.native = (1920, 1080, 60);
	let mut ctl = Controller::new(cfg);
	let mut path = Path::new(Profile { name: "bloat", capacity_kbps: 5000, delay_ms: 20.0, loss: 0.0 });
	path.queue_max_bits = 5_000_000.0; // 1 s of buffering
	let mut first_cut = None;
	let mut first_loss = None;
	for w in 0..30 {
		let kbps = ctl.encoder_kbps();
		let s = path.window(&mut ctl, kbps, w as f64 * 2000.0);
		if s.lost > 0 && first_loss.is_none() {
			first_loss = Some(w);
		}
		let d = ctl.tick(&s);
		if d.bitrate.is_some() && first_cut.is_none() {
			first_cut = Some((w, d.reason));
		}
	}
	let (w, reason) = first_cut.expect("a cut must happen");
	assert!(reason.contains("delay") || reason.contains("rtt"), "cut by the delay path, got: {reason}");
	assert!(first_loss.map_or(true, |l| w < l), "cut at {w} must precede the first loss at {first_loss:?}");
}

#[test]
fn random_loss_softens_via_recovery_and_fec_without_collapsing_the_rate() {
	let r = run(PROFILES[6], VCodec::H264, 20_000, 200); // 2 Mbit, 3 % loss, 120 ms
	assert_eq!(r.recovery_at.map(|w| w <= 2), Some(true), "recovery flip: {:?}", r.recovery_at);
	assert!(r.target_last >= 1000, "rate collapsed to {}", r.target_last);
	assert!(r.max_fec_overhead > 0.0 && r.max_fec_overhead <= fec_policy::RS_MAX_RATIO, "fec {}", r.max_fec_overhead);
	let _ = adapt::FLOOR_KBPS;
}

/// Trace one profile window by window (`cargo test --test adapt_scenarios trace -- --ignored
/// --nocapture`), for tuning.
#[test]
#[ignore]
fn trace_2mbit_3pct() {
	let profile = PROFILES[6];
	let mut cfg = Config::new(VCodec::H264, 20_000);
	cfg.native = (1920, 1080, 60);
	cfg.ir_capable = true;
	cfg.ladder = true;
	let mut ctl = Controller::new(cfg);
	let mut path = Path::new(profile);
	for w in 0..60 {
		let share = fec_policy::encoder_share_ratio(path.fec_ratio);
		let wire = (ctl.encoder_kbps() as f32 / share) as u32;
		let s = path.window(&mut ctl, wire, w as f64 * 2000.0);
		let d = ctl.tick(&s);
		let sig = ctl.last();
		println!(
			"w{w:3} wire={wire:5} loss={:5.1}% eff={:5.1}% rtt={:5.0} exc={:5.0} delay={:?} fec={:.2} -> target={} enc={} point={} ceil={} {}",
			sig.loss * 100.0, sig.eff_loss * 100.0, sig.rtt_ms, sig.excess_ms, sig.delay, path.fec_ratio,
			ctl.target_kbps(), ctl.encoder_kbps(), ctl.point().label(), ctl.ceiling(), d.reason
		);
	}
}
