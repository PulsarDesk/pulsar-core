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
	/// Report goodput (`Sample::recv_bytes` / `window_ms`, video packets only) like the
	/// 2026-11-09 clients; off = the older samples without it.
	measured: bool,
	/// The host sizes parity from the reports (`fec_policy`); off = no parity at all.
	fec: bool,
	/// Seeded xorshift state: when set, the random loss of each window scatters binomially
	/// around `p.loss` (the per-window spread of a real ~1000-packet window) instead of
	/// being exactly `p.loss` — deterministic loss hides thresholds that sit inside it.
	rng: Option<u64>,
}

impl Path {
	fn new(p: Profile) -> Self {
		let queue_max_bits = p.capacity_kbps as f64 * 1000.0 * 0.25; // 250 ms of buffering
		Self { p, queue_bits: 0.0, queue_max_bits, fec_ratio: 0.0, clean_windows: 0, measured: false, fec: true, rng: None }
	}

	/// A measured path (goodput reported) with `queue_s` seconds of buffering.
	fn measured(p: Profile, queue_s: f64) -> Self {
		let mut path = Self::new(p);
		path.queue_max_bits = p.capacity_kbps as f64 * 1000.0 * queue_s;
		path.measured = true;
		path
	}

	/// Uniform [0, 1) from the seeded xorshift.
	fn uniform(state: &mut u64) -> f64 {
		*state ^= *state << 13;
		*state ^= *state >> 7;
		*state ^= *state << 17;
		(*state >> 11) as f64 / (1u64 << 53) as f64
	}

	/// This window's random loss: `p.loss`, or a binomial draw around it over `n` packets
	/// (normal approximation) when the path is seeded.
	fn random_loss(&mut self, n: f64) -> f64 {
		let p = self.p.loss as f64;
		let Some(state) = self.rng.as_mut() else { return p };
		if p <= 0.0 {
			return 0.0;
		}
		let u1 = Self::uniform(state).max(1e-12);
		let u2 = Self::uniform(state);
		let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
		(p + z * (p * (1.0 - p) / n.max(1.0)).sqrt()).clamp(0.0, 0.95)
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
		let random = self.random_loss(pkts);
		let raw_loss = (overflow + random).min(0.95);
		// Reed-Solomon parity repairs RANDOM loss up to `m` per frame: nearly all of it when
		// the ratio covers the loss, a shrinking share when the loss exceeds the ratio.
		let repaired = if self.fec_ratio > 0.0 && random > 0.0 {
			let cover = if random <= self.fec_ratio as f64 { 0.95 } else { 0.5 * self.fec_ratio as f64 / random };
			random * cover
		} else {
			0.0
		};
		let loss = (raw_loss - repaired).max(0.0);
		let s = if self.measured {
			// Only video packets count (and carry bytes); parity is the rest of the wire.
			let video = pkts / (1.0 + self.fec_ratio as f64);
			let lost = (video * loss).round() as u32;
			let recv = (video as u32).saturating_sub(lost);
			Sample {
				recv,
				lost,
				rtt_ms: rtt_samples,
				fec_ok: (video * repaired) as u32,
				recv_bytes: recv as u64 * 1100,
				window_ms: 2000,
				..Default::default()
			}
		} else {
			let lost = (pkts * loss).round() as u32;
			Sample {
				recv: (pkts as u32).saturating_sub(lost),
				lost,
				rtt_ms: rtt_samples,
				fec_ok: (pkts * repaired) as u32,
				..Default::default()
			}
		};
		if !self.fec {
			return s;
		}
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

	/// One window at the controller's own rate (encoder + the parity share); ticks it.
	fn step(&mut self, ctl: &mut Controller, w: u32) -> (Sample, adapt::Decision) {
		let share = fec_policy::encoder_share_ratio(self.fec_ratio);
		let wire = (ctl.encoder_kbps() as f32 / share) as u32;
		let s = self.window(ctl, wire, w as f64 * 2000.0);
		let d = ctl.tick(&s);
		(s, d)
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
	/// First window the host sent parity in.
	fec_on_at: Option<u32>,
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
		fec_on_at: None,
	};
	let settle = windows / 3;
	for w in 0..windows {
		let share = fec_policy::encoder_share_ratio(path.fec_ratio);
		let wire = (ctl.encoder_kbps() as f32 / share) as u32;
		r.max_fec_overhead = r.max_fec_overhead.max(path.fec_ratio);
		if path.fec_ratio > 0.0 && r.fec_on_at.is_none() {
			r.fec_on_at = Some(w);
		}
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
			// brought the measured loss under the flip threshold (the 1 % profile; since the
			// cautious no-memory start any random-loss profile whose first loss is only the
			// random one, repaired by parity from the next window on).
			if let Some(first) = r.first_loss_at {
				let fec_repaired = profile.loss > 0.0 && r.fec_on_at.is_some_and(|f| f <= first + 1);
				if (profile.loss >= 0.03 || profile.loss == 0.0) && !fec_repaired {
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
	// Over the pipe from the first window (a remembered rate; no memory starts under it).
	cfg.start_kbps = 6000;
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

// ── Poor-link scenarios (2026-11-09) ────────────────────────────────────────────────────
//
// The field case: a laptop on USB-tethered mobile internet (~10 Mbit, 78 ms) to an RTX 3080
// host, saved picks 1080p · 120 fps · 15 Mbit, game mode — 0–3 fps, the rate halved every
// window down to 300 kbit/s and stayed there. These run with measured goodput, like the
// desktop client since then.

/// A game-mode session as `play.rs` seeds it: HEVC, 1080p120 native, the auto cap for it,
/// the game floors (60 fps, 720p), no per-peer memory; optional saved picks.
fn game_session(user_cap: u32, pins: Option<((u32, u32), u32)>) -> Controller {
	let mut cfg = Config::new(VCodec::H265, 25_200);
	cfg.native = (1920, 1080, 120);
	cfg.ladder = true;
	cfg.min_fps = 60;
	cfg.min_height = 720;
	let mut ctl = Controller::new(cfg);
	if user_cap > 0 {
		ctl.set_user_cap(user_cap);
	}
	if let Some((res, fps)) = pins {
		ctl.set_pins(Some(res), Some(fps));
	}
	ctl
}

const TETHER: Profile = Profile { name: "tether-10mbit-78ms", capacity_kbps: 10_000, delay_ms: 39.0, loss: 0.0 };

/// (a) The field case: starts cautiously, converges on a point ≥ 720p60 at ~6–9 Mbit under
/// the 1080p120 pick without collapsing to the floor, and says it is under the pick.
#[test]
fn tethered_10mbit_link_settles_under_the_pick_without_collapsing() {
	let mut ctl = game_session(15_000, Some(((1920, 1080), 120)));
	assert!(ctl.target_kbps() <= adapt::START_KBPS_NO_MEMORY, "start {}", ctl.target_kbps());
	assert_eq!(ctl.points()[0].label(), "1080p120", "the pick is the top rung");
	let mut path = Path::measured(TETHER, 0.25);
	let mut late_changes = 0;
	for w in 0..150u32 {
		let (_, d) = path.step(&mut ctl, w);
		let p = ctl.point();
		if w >= 20 {
			assert!(p.height >= 720 && p.fps >= 60, "w{w}: {}", p.label());
			assert!(ctl.target_kbps() >= 5_000, "w{w}: collapsed to {} ({})", ctl.target_kbps(), d.reason);
			// Under the pick ⇔ a shortfall to tell the user about.
			let short = ctl.pick_shortfall();
			if p != ctl.points()[0] {
				let (pick, cur) = short.unwrap_or_else(|| panic!("w{w}: no shortfall at {}", p.label()));
				assert_eq!((pick.label(), cur), ("1080p120".to_string(), p));
			} else {
				assert!(short.is_none());
			}
		}
		if w >= 60 && d.point.is_some() {
			late_changes += 1;
		}
	}
	let t = ctl.target_kbps();
	assert!((5_500..=10_000).contains(&t), "settled at {t} on {}", ctl.point().label());
	assert!(late_changes <= 2, "{late_changes} point changes after settling");
}

/// (b) 35 % random (non-congestive) loss on a 20 Mbit link, no parity: cutting doesn't
/// lower the loss, so the rate must not ride it down to the floor.
#[test]
fn heavy_random_loss_does_not_collapse_the_rate() {
	let mut ctl = game_session(0, None);
	let mut path = Path::measured(Profile { name: "20mbit-35pct", capacity_kbps: 20_000, delay_ms: 20.0, loss: 0.35 }, 0.25);
	path.fec = false;
	let floor = adapt::rate_floor_for_fps(60);
	let mut min_late = u32::MAX;
	for w in 0..150u32 {
		path.step(&mut ctl, w);
		if w >= 10 {
			min_late = min_late.min(ctl.target_kbps());
		}
	}
	assert!(min_late >= 2_000 && min_late > floor * 2, "rate collapsed to {min_late}");
	assert!(ctl.target_kbps() >= 3_000, "ended at {}", ctl.target_kbps());
	assert!(ctl.loss_is_non_responsive(), "the loss was judged non-congestive");
}

/// (b2) The same 35 % random loss with its real window-to-window (binomial) scatter on a
/// 10 Mbit link: the learned floor must not turn ordinary scatter into "loss over noise"
/// that keeps the climb sitting at the cautious start for a minute or more.
#[test]
fn heavy_random_loss_with_binomial_scatter_still_climbs() {
	let mut slow = 0;
	let mut late_sum = 0u64;
	let seeds = 20u64;
	for seed in 1..=seeds {
		let mut ctl = game_session(15_000, Some(((1920, 1080), 120)));
		let mut path = Path::measured(Profile { name: "10mbit-35pct", capacity_kbps: 10_000, delay_ms: 39.0, loss: 0.35 }, 0.25);
		path.fec = false;
		path.rng = Some(0x9E37_79B9_7F4A_7C15u64.wrapping_mul(seed));
		let mut first = None;
		for w in 0..150u32 {
			path.step(&mut ctl, w);
			if first.is_none() && ctl.target_kbps() > adapt::START_KBPS_NO_MEMORY {
				first = Some(w);
			}
			if w >= 50 {
				late_sum += ctl.target_kbps() as u64;
			}
		}
		if !matches!(first, Some(w) if w <= 20) {
			slow += 1;
		}
	}
	let late_avg = late_sum / (seeds * 100);
	assert!(slow <= 2, "{slow} of {seeds} seeds sat at the start for 40 s or more");
	assert!((6_500..=10_500).contains(&late_avg), "late average {late_avg}");
}

/// (b3) A 10 Mbit policer (5 ms of buffer) with 35 % random loss on top, a 1080p120 /
/// 15 Mbit pick: a probe-down from over the policer is judged on the loss OVER the learned
/// floor. Judged on the raw loss it always read "did not follow" and looped 11.25 → 9 →
/// 11.25 Mbit every 5 windows, 3 of 5 windows 12 % over the capacity.
#[test]
fn a_policer_under_heavy_random_loss_does_not_loop_over_its_rate() {
	let mut ctl = game_session(15_000, Some(((1920, 1080), 120)));
	let mut path = Path::measured(Profile { name: "policer-10mbit-35pct", capacity_kbps: 10_000, delay_ms: 39.0, loss: 0.35 }, 0.005);
	path.fec = false;
	let mut restores = 0;
	let mut over = 0;
	for w in 0..150u32 {
		let (_, d) = path.step(&mut ctl, w);
		if w >= 40 {
			if d.reason.contains("did not follow") {
				restores += 1;
			}
			if ctl.target_kbps() > 10_500 {
				over += 1;
			}
		}
	}
	assert!(restores <= 1, "{restores} probe-down/restore pairs after settling");
	assert!(over <= 12, "{over} late windows over the capacity");
	assert!(ctl.noise_loss() <= 0.36, "the floor ratcheted to {}", ctl.noise_loss());
	assert!(ctl.target_kbps() >= 7_000, "settled at {}", ctl.target_kbps());
}

/// (c) A 3 Mbit policer (no queue: everything over the rate is dropped): converges near
/// 0.85 × 3 Mbit, no sawtooth down to the floor.
#[test]
fn a_policer_converges_near_its_rate_without_a_sawtooth() {
	let mut ctl = game_session(0, None);
	let mut path = Path::measured(Profile { name: "policer-3mbit", capacity_kbps: 3_000, delay_ms: 30.0, loss: 0.0 }, 0.005);
	let mut lo = u32::MAX;
	let mut hi = 0;
	let mut lossy = 0;
	for w in 0..200u32 {
		let (s, _) = path.step(&mut ctl, w);
		if w >= 40 {
			lo = lo.min(ctl.target_kbps());
			hi = hi.max(ctl.target_kbps());
			if s.lost > 0 {
				lossy += 1;
			}
		}
	}
	assert!(lo >= 1_800, "sawtooth down to {lo}");
	assert!(hi <= 3_600, "rode over the policer: {hi}");
	assert!((2_000..=3_100).contains(&ctl.target_kbps()), "settled at {}", ctl.target_kbps());
	assert!(lossy <= 12, "{lossy} lossy windows after settling");
}

/// (d) A clean LAN without memory: the cautious start costs seconds, not the session — the
/// top rung and the cap within ~20 s.
#[test]
fn a_clean_lan_reaches_the_top_rung_and_the_cap_fast_from_the_cautious_start() {
	for cap in [25_200u32, 100_000] {
		let mut cfg = Config::new(VCodec::H265, cap);
		cfg.native = (1920, 1080, 120);
		cfg.ladder = true;
		cfg.min_fps = 60;
		cfg.min_height = 720;
		let mut ctl = Controller::new(cfg);
		assert_eq!(ctl.target_kbps(), adapt::START_KBPS_NO_MEMORY);
		let mut path = Path::measured(Profile { name: "lan-100mbit", capacity_kbps: 100_000, delay_ms: 1.0, loss: 0.0 }, 0.05);
		let (mut top_at, mut cap_at) = (None, None);
		for w in 0..20u32 {
			path.step(&mut ctl, w);
			if top_at.is_none() && ctl.point().label() == "1080p120" {
				top_at = Some(w);
			}
			if cap_at.is_none() && ctl.target_kbps() as f32 >= cap as f32 * 0.95 {
				cap_at = Some(w);
			}
		}
		assert!(matches!(top_at, Some(w) if w <= 10), "cap {cap}: top rung at {top_at:?}");
		assert!(matches!(cap_at, Some(w) if w <= 10), "cap {cap}: cap at {cap_at:?} ({})", ctl.target_kbps());
	}
}

/// (e) The rate never sits under the current point's minimum (only the bottom rung may go
/// lower), nor under the fps-aware floor — a 1080p120 encoder never gets 300 kbit/s.
#[test]
fn the_rate_never_goes_under_the_point_floor() {
	let mut ctl = game_session(15_000, Some(((1920, 1080), 120)));
	let mut path = Path::measured(Profile { name: "1mbit-78ms", capacity_kbps: 1_000, delay_ms: 39.0, loss: 0.02 }, 0.25);
	for w in 0..120u32 {
		path.step(&mut ctl, w);
		let p = ctl.point();
		let t = ctl.target_kbps();
		assert!(t >= adapt::rate_floor_for_fps(p.fps), "w{w}: {t} at {}", p.label());
		assert!(t >= ctl.rate_floor_kbps(), "w{w}: {t} under the floor {}", ctl.rate_floor_kbps());
		if p != *ctl.points().last().unwrap() {
			assert!(t >= p.min_kbps, "w{w}: {t} under {}'s minimum {}", p.label(), p.min_kbps);
		}
	}
	assert_eq!(ctl.point().label(), "720p60", "a 1 Mbit path ends on the bottom rung");
	assert!(ctl.pick_shortfall().is_some());
	assert_eq!(adapt::rate_floor_for_fps(120), 1_440, "≥ 1.5 KB a frame at 120 fps");
	assert_eq!(adapt::rate_floor_for_fps(10), adapt::FLOOR_KBPS);
}

/// (f) Stuck at the floor (a path under it), then the path recovers but keeps a steady 10 %
/// random loss: the controller leaves the floor within ~60 s, whatever ceiling it punished.
#[test]
fn the_floor_is_escaped_within_a_minute() {
	let mut ctl = game_session(0, None);
	let mut path = Path::measured(Profile { name: "400kbit", capacity_kbps: 400, delay_ms: 39.0, loss: 0.0 }, 0.25);
	path.fec = false;
	for w in 0..60u32 {
		path.step(&mut ctl, w);
	}
	let floor = adapt::rate_floor_for_fps(60);
	assert!(ctl.target_kbps() <= floor * 2, "at the floor: {}", ctl.target_kbps());
	let mut path = Path::measured(Profile { name: "20mbit-10pct", capacity_kbps: 20_000, delay_ms: 39.0, loss: 0.10 }, 0.25);
	path.fec = false;
	let mut out_at = None;
	for w in 60..150u32 {
		path.step(&mut ctl, w);
		if out_at.is_none() && ctl.target_kbps() >= floor * 3 {
			out_at = Some(w - 60);
		}
	}
	assert!(matches!(out_at, Some(w) if w <= 30), "left the floor after {out_at:?} windows");
	assert!(ctl.target_kbps() >= 5_000, "and kept climbing: {}", ctl.target_kbps());
}

/// The old samples (no goodput) still run the pre-2026-11-09 paths: a severe loss halves.
#[test]
fn unmeasured_samples_keep_the_old_halving() {
	let mut cfg = Config::new(VCodec::H264, 8_000);
	cfg.start_kbps = 8_000;
	let mut ctl = Controller::new(cfg);
	for _ in 0..3 {
		ctl.tick(&Sample { recv: 1000, lost: 0, rtt_ms: vec![20.0; 4], ..Default::default() });
	}
	let d = ctl.tick(&Sample { recv: 600, lost: 400, rtt_ms: vec![20.0; 4], ..Default::default() });
	assert_eq!(d.bitrate, Some(4_000), "{d:?}");
	assert_eq!(ctl.last().goodput_kbps, 0.0);
}

/// The whole matrix again with measured goodput: the new loss rules must not hurt the
/// clean / congested profiles the old ones were tuned on.
#[test]
fn matrix_with_measured_goodput_still_settles_under_capacity() {
	for &profile in PROFILES {
		for codec in [VCodec::H264, VCodec::H265] {
			let mut cfg = Config::new(codec, 20_000);
			cfg.native = (1920, 1080, 60);
			cfg.ir_capable = true;
			cfg.ladder = true;
			let mut ctl = Controller::new(cfg);
			let mut path = Path::measured(profile, 0.25);
			let mut over_late = 0;
			for w in 0..300u32 {
				let share = fec_policy::encoder_share_ratio(path.fec_ratio);
				let wire = (ctl.encoder_kbps() as f32 / share) as u32;
				if w >= 100 && wire > profile.capacity_kbps {
					over_late += 1;
				}
				let s = path.window(&mut ctl, wire, w as f64 * 2000.0);
				ctl.tick(&s);
			}
			let tag = format!("{} {codec:?}", profile.name);
			let cap_eff = profile.capacity_kbps.min(20_000) as f32;
			assert!(over_late <= 6, "{tag}: {over_late} overshoot windows (target {})", ctl.target_kbps());
			assert!(ctl.target_kbps() as f32 <= cap_eff * 1.05, "{tag}: {}", ctl.target_kbps());
			if profile.loss < 0.05 {
				assert!(ctl.target_kbps() as f32 >= cap_eff * 0.40, "{tag}: collapsed to {}", ctl.target_kbps());
			}
		}
	}
}

// ── Packet-rate policer (2026-11-09 field run "pol10") ──────────────────────────────────
//
// Pi client ← RTX host through a downlink policer of 1040 packets/s with a 60-packet burst
// (~9.6 Mbit/s on the wire, parity and retransmits included), 1 % random loss, 78 ms RTT,
// high-motion content (the encoder fills its target). The controller climbed straight back
// to the rate that had just failed after one clean window, and every probe-down that cured
// the loss read "inconclusive (fec changed)" because the parity share moved under it (15 →
// 50 %): 1–2 s of near-frozen video every ~10 s for the first minute.

/// The policer path, modelled per frame like the field client counts it (`play/hold.rs`):
/// each frame's video packets, then its Reed-Solomon parity, pass a token bucket and the
/// random loss; a frame whose parity can't rebuild its losses NACKs them, and the
/// retransmits cross the bucket one RTT later. `recv` counts arrivals (retransmits included,
/// FEC-rebuilt packets not), `lost` what is still missing — exactly the desktop's counters.
/// The host sizes parity with its own policy (`FecPolicy`, WAN) from the stats it gets.
struct PacketPolicer {
	rate_pps: f64,
	burst: f64,
	tokens: f64,
	pkt_bytes: u32,
	loss: f64,
	rtt_ms: f64,
	fps: u32,
	rng: u64,
	fec: fec_policy::FecPolicy,
	ratio: f32,
	/// Encoder bits not yet packetised (a fractional packet carries over to the next frame).
	carry_bits: f64,
	/// Retransmits waiting out their NACK round trip: (frame due, packets).
	rtx: std::collections::VecDeque<(u64, u32)>,
	frame: u64,
	/// Time (ms) the bucket was last refilled.
	last_ms: f64,
}

/// One policer window: the sample the client reports, and the path's true pre-repair loss
/// (first-copy video packets dropped / sent).
struct PolWindow {
	sample: Sample,
	raw: f64,
}

impl PacketPolicer {
	fn pol10(seed: u64) -> Self {
		Self {
			rate_pps: 1040.0,
			burst: 60.0,
			tokens: 60.0,
			pkt_bytes: 1150,
			loss: 0.01,
			rtt_ms: 78.0,
			fps: 90,
			rng: 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(seed) | 1,
			fec: fec_policy::FecPolicy::new(true),
			ratio: fec_policy::WAN_FLOOR,
			carry_bits: 0.0,
			rtx: std::collections::VecDeque::new(),
			frame: 0,
			last_ms: 0.0,
		}
	}

	/// The bottleneck as a wire rate (kbit/s).
	fn capacity_kbps(&self) -> f64 {
		self.rate_pps * self.pkt_bytes as f64 * 8.0 / 1000.0
	}

	/// One packet sent at `t_ms` through the bucket (refilled continuously) and the random loss.
	fn pass(&mut self, t_ms: f64) -> bool {
		self.tokens = (self.tokens + self.rate_pps * (t_ms - self.last_ms).max(0.0) / 1000.0).min(self.burst);
		self.last_ms = t_ms;
		if self.tokens < 1.0 {
			return false;
		}
		self.tokens -= 1.0;
		Path::uniform(&mut self.rng) >= self.loss
	}

	/// Run one 2 s window at the controller's encoder rate plus the host's parity; feeds the
	/// delay estimator (a policer has no queue: flat arrivals) and updates the host's parity.
	fn window(&mut self, ctl: &mut Controller, w: u32) -> PolWindow {
		let frame_ms = 1000.0 / self.fps as f64;
		let frames = (2000.0 / frame_ms).round() as u32;
		let pkt_bits = self.pkt_bytes as f64 * 8.0;
		let rtt_frames = (self.rtt_ms / frame_ms).ceil() as u64;
		let enc_bits = ctl.encoder_kbps() as f64 * frame_ms;
		let (mut recv, mut lost, mut nack_sent, mut nack_ok, mut fec_ok) = (0u32, 0u32, 0u32, 0u32, 0u32);
		let (mut sent, mut dropped, mut parity_sent, mut rtx_sent) = (0u32, 0u32, 0u64, 0u64);
		for i in 0..frames {
			self.frame += 1;
			let t = w as f64 * 2000.0 + i as f64 * frame_ms;
			// The frame's packets: the retransmits that are due (the pacer sends them first),
			// then the video, then its parity.
			let mut due = 0u32;
			while self.rtx.front().is_some_and(|&(at, _)| at <= self.frame) {
				due += self.rtx.pop_front().unwrap().1;
			}
			// High-motion content: the frame sizes scatter ±40 % around the target's average.
			let size = 0.6 + 0.8 * Path::uniform(&mut self.rng);
			self.carry_bits += enc_bits * size;
			let k = (self.carry_bits / pkt_bits).floor() as u32;
			self.carry_bits -= k as f64 * pkt_bits;
			let m = if self.ratio > 0.0 && k > 0 { (k as f32 * self.ratio).ceil() as u32 } else { 0 };
			// The host's pacer lets a frame out as one burst (its 16 ms budget at 2× the rate
			// holds a whole frame): ~0.1 ms per packet at line rate.
			let mut slot = 0u32;
			let mut send = |p: &mut Self| {
				slot += 1;
				p.pass(t + 0.1 * (slot - 1) as f64)
			};
			rtx_sent += due as u64;
			parity_sent += m as u64;
			for _ in 0..due {
				if send(self) {
					recv += 1;
					nack_ok += 1;
					lost = lost.saturating_sub(1);
				}
			}
			if k > 0 {
				let k_ok = (0..k).filter(|_| send(self)).count() as u32;
				let m_ok = (0..m).filter(|_| send(self)).count() as u32;
				let k_lost = k - k_ok;
				sent += k;
				dropped += k_lost;
				recv += k_ok;
				if k_lost > 0 {
					if m_ok >= k_lost {
						fec_ok += k_lost;
					} else {
						lost += k_lost;
						nack_sent += k_lost;
						self.rtx.push_back((self.frame + rtt_frames, k_lost));
					}
				}
			}
			ctl.on_frame((t * 90.0) as u32, t + self.rtt_ms / 2.0);
		}
		let total = recv + lost;
		let post = if total > 0 { lost as f32 / total as f32 } else { 0.0 };
		let sample = Sample {
			recv,
			lost,
			rtt_ms: vec![self.rtt_ms as f32; 4],
			nack_sent,
			nack_ok,
			fec_ok,
			recv_bytes: recv as u64 * self.pkt_bytes as u64,
			window_ms: 2000,
		};
		let bytes = self.pkt_bytes as u64;
		self.ratio = self.fec.on_window(&fec_policy::FecWindow {
			loss: post,
			nack_ok,
			fec_ok,
			sent_pkts: sent as u64,
			window_ms: 2000,
			kbps: ctl.encoder_kbps(),
			fps: self.fps,
			rtt_ms: self.rtt_ms as f32,
			video_bytes: sent as u64 * bytes,
			parity_pkts: parity_sent,
			parity_bytes: parity_sent * bytes,
			rtx_bytes: rtx_sent * bytes,
			..Default::default()
		});
		ctl.set_fec_overhead(self.ratio);
		PolWindow { sample, raw: if sent > 0 { dropped as f64 / sent as f64 } else { 0.0 } }
	}
}

/// The field session's controller: HEVC, 1080p90 native, the 15 Mbit auto cap, game floors,
/// no per-peer memory.
fn pol10_session() -> Controller {
	let mut cfg = Config::new(VCodec::H265, 15_000);
	cfg.native = (1920, 1080, 90);
	cfg.ladder = true;
	cfg.min_fps = 60;
	cfg.min_height = 720;
	Controller::new(cfg)
}

/// (g) The pol10 field run: after the opening overshoot (the first 30 s) the controller
/// settles under the policer — at most one loss episode (a run of windows with pre-repair
/// loss over 3 %) per minute, each one short, never under 70 % of the bottleneck — instead
/// of re-hitting the failed rate every ~10 s (the field) or sitting over the policer with
/// its overflow learned as "noise" (what the old rules do in this model: 40–70 % pre-repair
/// loss, repaired to ~3 %, for minutes).
#[test]
fn a_packet_policer_is_not_re_hit_every_few_seconds() {
	for seed in 1..=6u64 {
		let mut ctl = pol10_session();
		let mut path = PacketPolicer::pol10(seed);
		let cap = path.capacity_kbps();
		let mut episodes: Vec<u32> = Vec::new();
		let (mut lossy_windows, mut run, mut longest) = (0u32, 0u32, 0u32);
		let mut lossy_prev = false;
		let mut min_late = u32::MAX;
		let mut sum_late = 0u64;
		let mut trace = Vec::new();
		for w in 0..300u32 {
			let pw = path.window(&mut ctl, w);
			let d = ctl.tick(&pw.sample);
			trace.push(format!(
				"w{w} raw={:.1}% loss={:.1}% fec={:.2} -> target={} ceil={} {}",
				pw.raw * 100.0,
				ctl.last().loss * 100.0,
				path.ratio,
				ctl.target_kbps(),
				ctl.ceiling(),
				d.reason
			));
			let lossy = pw.raw > 0.03;
			if w >= 15 {
				if lossy && !lossy_prev {
					episodes.push(w);
				}
				run = if lossy { run + 1 } else { 0 };
				longest = longest.max(run);
				lossy_windows += lossy as u32;
				min_late = min_late.min(ctl.target_kbps());
				sum_late += ctl.target_kbps() as u64;
			}
			lossy_prev = lossy;
		}
		let tail = || trace.join("\n");
		for pair in episodes.windows(2) {
			assert!(pair[1] - pair[0] >= 30, "seed {seed}: loss episodes at {episodes:?} (windows)\n{}", tail());
		}
		assert!(longest <= 2, "seed {seed}: a loss episode lasted {longest} windows\n{}", tail());
		assert!(lossy_windows <= 10, "seed {seed}: {lossy_windows} lossy windows after settling\n{}", tail());
		assert!(min_late as f64 >= cap * 0.70, "seed {seed}: fell to {min_late} under a {cap:.0} kbit policer\n{}", tail());
		let avg = sum_late as f64 / 285.0;
		assert!(avg >= cap * 0.85, "seed {seed}: average {avg:.0} under a {cap:.0} kbit policer\n{}", tail());
	}
}

/// Trace the pol10 policer window by window (`PULSAR_SEED=n cargo test --test adapt_scenarios
/// trace_pol10 -- --ignored --nocapture`), for tuning. `PULSAR_FIXED=kbps` pins the target
/// before every window (what the path does at one rate); `PULSAR_LOSS_PM=n` sets the random
/// loss (‰; the field's second run had 80).
#[test]
#[ignore]
fn trace_pol10() {
	let env = |k: &str| std::env::var(k).ok().and_then(|s| s.parse::<u32>().ok());
	let seed = env("PULSAR_SEED").unwrap_or(1) as u64;
	let mut ctl = pol10_session();
	let mut path = PacketPolicer::pol10(seed);
	if let Some(pm) = env("PULSAR_LOSS_PM") {
		path.loss = pm as f64 / 1000.0;
	}
	for w in 0..300u32 {
		if let Some(kbps) = env("PULSAR_FIXED") {
			ctl.set_target(kbps);
		}
		let pw = path.window(&mut ctl, w);
		let s = &pw.sample;
		let d = ctl.tick(s);
		println!(
			"w{w:3} recv={} lost={} nack={} fec_ok={} raw={:5.1}% loss={:5.1}% good={:6.0} fec={:.2} -> target={} enc={} point={} ceil={} wait={} {}",
			s.recv, s.lost, s.nack_ok, s.fec_ok, pw.raw * 100.0, ctl.last().loss * 100.0, ctl.last().goodput_kbps,
			path.ratio, ctl.target_kbps(), ctl.encoder_kbps(), ctl.point().label(), ctl.ceiling(), ctl.probe_wait(), d.reason
		);
	}
}

/// Trace a poor-link scenario window by window (`PULSAR_TRACE=tether|random|policer|floor
/// cargo test --test adapt_scenarios trace_poor -- --ignored --nocapture`), for tuning.
#[test]
#[ignore]
fn trace_poor_link() {
	let which = std::env::var("PULSAR_TRACE").unwrap_or_default();
	let (mut ctl, mut path) = match which.as_str() {
		"random" => {
			let mut p = Path::measured(Profile { name: "r", capacity_kbps: 20_000, delay_ms: 20.0, loss: 0.35 }, 0.25);
			p.fec = false;
			(game_session(0, None), p)
		}
		"policer" => (game_session(0, None), Path::measured(Profile { name: "p", capacity_kbps: 3_000, delay_ms: 30.0, loss: 0.0 }, 0.005)),
		"floor" => {
			let mut p = Path::measured(Profile { name: "f", capacity_kbps: 400, delay_ms: 39.0, loss: 0.0 }, 0.25);
			p.fec = false;
			(game_session(0, None), p)
		}
		_ => (game_session(15_000, Some(((1920, 1080), 120))), Path::measured(TETHER, 0.25)),
	};
	for w in 0..150u32 {
		if which == "floor" && w == 60 {
			path = Path::measured(Profile { name: "f2", capacity_kbps: 20_000, delay_ms: 39.0, loss: 0.10 }, 0.25);
			path.fec = false;
		}
		let (s, d) = path.step(&mut ctl, w);
		let sig = ctl.last();
		println!(
			"w{w:3} recv={} lost={} loss={:5.1}% eff={:5.1}% good={:6.0} send={:6.0} exc={:4.0} delay={:?} fec={:.2} -> target={} enc={} point={} ceil={} nr={} short={:?} {}",
			s.recv, s.lost, sig.loss * 100.0, sig.eff_loss * 100.0, sig.goodput_kbps, sig.send_kbps, sig.excess_ms, sig.delay,
			path.fec_ratio, ctl.target_kbps(), ctl.encoder_kbps(), ctl.point().label(), ctl.ceiling(),
			ctl.loss_is_non_responsive(), ctl.pick_shortfall().map(|(_, c)| c.label()), d.reason
		);
	}
}
