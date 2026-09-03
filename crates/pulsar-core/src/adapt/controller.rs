//! The shared client-side adaptive controller (adaptive streaming Phases 0–3). One pure
//! decision function over 2 s windows of measurements, used by the desktop and mobile
//! apps alike — the transport (LAN / P2P / relay) only contributes a hard cap.
//!
//! Per window it produces a **target wire rate** (kbit/s) from loss, keepalive RTT and the
//! delay-gradient estimator, then picks an **operating point** (resolution × fps) from the
//! codec's ladder so that the target buys a clean picture, and the **encoder bitrate**
//! (the target minus the FEC parity share). It also flips the host's loss-recovery mode
//! once, on the first sustained loss.
//!
//! Rate rules (evidence in `docs/adaptive-streaming.md`):
//!
//! * **Delay first.** The trendline estimator's `Overuse` (queue filling) cuts ×0.85 right
//!   away — before loss; a keepalive-RTT excess over its baseline of ≥ 35 ms cuts ×0.7,
//!   ≥ 90 ms halves. No climb while either says the link is queued.
//! * **Loss.** Raw loss > 15 % halves at once. Sustained loss > 3 % (after the learned
//!   noise floor) with a queued link cuts ×0.7; with a *flat* link it is probed once
//!   (×0.8) and, if the loss does not follow the rate, learned as random loss that no
//!   longer drives the rate — netem-style loss softens the picture via recovery mode
//!   instead of collapsing it.
//! * **No sawtooth.** A rate that produced loss/bloat becomes a punished ceiling (×0.85).
//!   Climbing back to the ceiling is free (after 20 s clean), going past it is a probe
//!   allowed only after 60 s clean at the ceiling; every failed probe doubles that wait.
//! * **Startup fast.** For the first `startup_windows` a clean window climbs ×1.5 at once.
//!
//! Ladder rules: pick the highest point whose minimum fits `target × 0.85`; go **down** as
//! soon as the target is under the current point's minimum (resolution changes restart an
//! encoder, so at most one change per 2 windows unless the shortfall is severe); go **up**
//! one rung only after 20 s clean at the current point and `target ≥ next.min × 1.2`.

use super::delay::{DelayState, Trendline};
use super::fec_policy;
use super::ladder::{self, Point};
use crate::pipeline::VCodec;
use crate::service::LossRecovery;

/// Bitrate floor (kbit/s): what the lowest rung still needs to look like anything.
pub const FLOOR_KBPS: u32 = 300;
/// "Clean" window: loss below this (after the noise floor) counts toward a climb.
pub const LOSS_CLEAN: f32 = 0.005;
/// Sustained loss above this (after the noise floor) steps the rate down.
pub const LOSS_DOWN: f32 = 0.03;
/// Raw loss above this halves at once.
pub const LOSS_SEVERE: f32 = 0.15;
/// RTT excess (ms) under which the link counts as unqueued (climb allowed).
pub const RTT_OK_MS: f32 = 25.0;
/// RTT excess that cuts ×0.7 even at zero loss.
pub const RTT_EXCESS_MS: f32 = 35.0;
/// RTT excess that halves at once.
pub const RTT_BAD_MS: f32 = 90.0;
/// Consecutive clean windows before a climb toward the ceiling (20 s).
pub const CLEAN_WINDOWS_UP: u32 = 10;
/// Clean windows at the ceiling before the first probe past it (60 s).
pub const PROBE_WAIT_MIN: u32 = 30;
/// Upper bound for the doubled probe wait (16 min).
pub const PROBE_WAIT_MAX: u32 = 480;
/// A probe that draws loss/bloat within this many windows counts as failed.
pub const PROBE_PUNISH_WINDOWS: u32 = 10;
/// Minimum windows between gentle steps.
pub const COOLDOWN_WINDOWS: u32 = 3;
/// Raw loss above which a window is "lossy" for the recovery-mode flip.
pub const RECOVERY_LOSS: f32 = 0.005;
/// Consecutive lossy windows that flip the recovery mode (a window over `LOSS_DOWN` flips
/// at once).
pub const RECOVERY_WINDOWS: u32 = 2;
/// Windows a probe-down is observed before judging whether the loss followed the rate.
pub const PROBE_DOWN_WINDOWS: u32 = 2;
/// Consecutive windows of mild loss (between clean and down) on a flat, unqueued link
/// before that loss is learned as the path's noise floor (the residual after FEC/NACK).
pub const DEADBAND_LEARN_WINDOWS: u32 = 5;
/// Clean windows at a rate before it is remembered as the peer's "last good" rate (30 s).
pub const LAST_GOOD_WINDOWS: u32 = 15;
/// Ladder: margin under the target a point's minimum must fit (going up).
pub const POINT_MARGIN: f32 = 0.85;
/// Ladder: the next-higher point's minimum must be under `target / UP_HEADROOM`.
pub const UP_HEADROOM: f32 = 1.2;
/// Ladder: windows between resolution changes (each restarts an encoder).
pub const POINT_DEBOUNCE: u32 = 2;
/// Fast reflex: Overuse must persist this long (ms) before an out-of-window cut.
pub const FAST_OVERUSE_MS: f64 = 500.0;
/// Fast reflex: at most one cut per this many ms.
pub const FAST_COOLDOWN_MS: f64 = 1500.0;

/// One window's measurements.
#[derive(Clone, Debug, Default)]
pub struct Sample {
	/// Video RTP packets received in the window (incl. repaired ones).
	pub recv: u32,
	/// Video RTP packets still missing at the end of the window.
	pub lost: u32,
	/// Keepalive RTT samples that arrived during the window (ms), oldest first.
	pub rtt_ms: Vec<f32>,
	/// NACKs sent / answered in time (diagnostic, forwarded to the host).
	pub nack_sent: u32,
	pub nack_ok: u32,
	/// Packets rebuilt from FEC parity (diagnostic).
	pub fec_ok: u32,
}

/// Derived per-window signals (logs, HUD, client→host stats).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Signals {
	pub loss: f32,
	pub eff_loss: f32,
	pub rtt_ms: f32,
	pub excess_ms: f32,
	pub jitter_ms: f32,
	pub delay: DelayState,
	/// Gain-scaled delay trend (ms/ms × gain) — for logs.
	pub trend: f32,
}

/// What to actuate after a window. All `None` = hold.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decision {
	/// New encoder bitrate (kbit/s) — already net of the FEC share.
	pub bitrate: Option<u32>,
	/// New operating point (resolution × fps).
	pub point: Option<Point>,
	pub recovery: Option<LossRecovery>,
	pub reason: &'static str,
}

impl Decision {
	pub fn is_change(&self) -> bool {
		self.bitrate.is_some() || self.point.is_some() || self.recovery.is_some()
	}
}

/// Controller configuration for one session.
#[derive(Clone, Debug)]
pub struct Config {
	pub codec: VCodec,
	/// Host native capture `(w, h, fps)`; 0 = unknown (1920×1080@60 assumed).
	pub native: (u32, u32, u32),
	/// Hard cap (kbit/s): the user's pick, already clamped to a relay cap.
	pub cap_kbps: u32,
	/// Starting target (kbit/s); 0 = the cap. Seed with 60 % of the peer's last good rate.
	pub start_kbps: u32,
	/// Whether this client can resume on a partially intra-refreshed picture.
	pub ir_capable: bool,
	/// Run the resolution/fps ladder (false = bitrate only, e.g. a user-pinned size).
	pub ladder: bool,
	/// Windows of aggressive startup probing.
	pub startup_windows: u32,
}

impl Config {
	pub fn new(codec: VCodec, cap_kbps: u32) -> Self {
		Self {
			codec,
			native: (0, 0, 0),
			cap_kbps,
			start_kbps: 0,
			ir_capable: false,
			ladder: true,
			startup_windows: 5,
		}
	}
}

/// Map a wire codec id (`h264` / `h265` / `hevc` / `av1`) to the ladder's codec.
pub fn codec_from_wire(id: &str) -> VCodec {
	match id.trim().to_ascii_lowercase().as_str() {
		"h265" | "hevc" => VCodec::H265,
		"av1" => VCodec::Av1,
		_ => VCodec::H264,
	}
}

#[derive(Clone, Copy, Debug)]
struct ProbeDown {
	from: u32,
	pre_loss: f32,
	windows: u32,
	loss_acc: f32,
	/// FEC group size when the probe started: parity switching on/up mid-probe lowers the
	/// measured loss by itself, so the judgement is only valid when it stayed the same.
	fec_n: u8,
}

/// The controller state, carried across windows.
#[derive(Clone, Debug)]
pub struct Controller {
	cfg: Config,
	/// Target wire rate (kbit/s).
	target: u32,
	cap: u32,
	ceiling: u32,
	clean: u32,
	at_ceiling: u32,
	probe_wait: u32,
	probe_age: Option<u32>,
	since_step: u32,
	over: u32,
	lossy: u32,
	noise_loss: f32,
	probe_down: Option<ProbeDown>,
	rtt_base: f32,
	rtt_ema: f32,
	rtt_last: f32,
	/// Consecutive deadband windows (mild loss, flat link).
	deadband: u32,
	recovery: LossRecovery,
	startup_left: u32,
	windows: u32,
	last: Signals,
	// Ladder.
	points: Vec<Point>,
	point_idx: usize,
	point_age: u32,
	clean_at_point: u32,
	manual_point: bool,
	/// The encoder bitrate last decided (net of FEC).
	encoder_kbps: u32,
	fec_n: u8,
	// Delay estimator + fast reflex.
	delay: Trendline,
	last_fast_cut_ms: f64,
	// Memory.
	clean_at_target: u32,
	last_good: u32,
}

impl Controller {
	pub fn new(cfg: Config) -> Self {
		let cap = cfg.cap_kbps.max(FLOOR_KBPS);
		let start = if cfg.start_kbps == 0 { cap } else { cfg.start_kbps.clamp(FLOOR_KBPS, cap) };
		let points = ladder::build(cfg.codec, cfg.native.0, cfg.native.1, cfg.native.2);
		let point_idx = if cfg.ladder { ladder::pick(&points, start, POINT_MARGIN) } else { 0 };
		let mut c = Self {
			target: start,
			cap,
			ceiling: cap,
			clean: 0,
			at_ceiling: 0,
			probe_wait: PROBE_WAIT_MIN,
			probe_age: None,
			since_step: COOLDOWN_WINDOWS,
			over: 0,
			lossy: 0,
			noise_loss: 0.0,
			probe_down: None,
			rtt_base: 0.0,
			rtt_ema: 0.0,
			rtt_last: 0.0,
			deadband: 0,
			recovery: LossRecovery::Normal,
			startup_left: cfg.startup_windows,
			windows: 0,
			last: Signals::default(),
			points,
			point_idx,
			point_age: POINT_DEBOUNCE,
			clean_at_point: 0,
			manual_point: !cfg.ladder,
			encoder_kbps: 0,
			fec_n: 0,
			delay: Trendline::new(),
			last_fast_cut_ms: -1.0e9,
			clean_at_target: 0,
			last_good: 0,
			cfg,
		};
		c.encoder_kbps = c.encoder_rate();
		c
	}

	// ── Read-only views ─────────────────────────────────────────────────────────────

	pub fn target_kbps(&self) -> u32 {
		self.target
	}
	/// Encoder bitrate currently requested (net of the FEC share).
	pub fn encoder_kbps(&self) -> u32 {
		self.encoder_kbps
	}
	pub fn point(&self) -> Point {
		self.points[self.point_idx]
	}
	pub fn points(&self) -> &[Point] {
		&self.points
	}
	pub fn ceiling(&self) -> u32 {
		self.ceiling
	}
	pub fn probe_wait(&self) -> u32 {
		self.probe_wait
	}
	pub fn noise_loss(&self) -> f32 {
		self.noise_loss
	}
	pub fn recovery(&self) -> LossRecovery {
		self.recovery
	}
	pub fn windows(&self) -> u32 {
		self.windows
	}
	pub fn last(&self) -> Signals {
		self.last
	}
	pub fn fec_n(&self) -> u8 {
		self.fec_n
	}
	/// The most recent rate that stayed clean for `LAST_GOOD_WINDOWS` (0 = none yet). The
	/// app persists it per peer and seeds the next session with 60 % of it.
	pub fn last_good_kbps(&self) -> u32 {
		self.last_good
	}
	/// The recovery mode this client asks for once the path proves lossy.
	pub fn preferred_recovery(&self) -> LossRecovery {
		if self.cfg.ir_capable {
			LossRecovery::IntraRefresh
		} else {
			LossRecovery::ShortGop
		}
	}

	// ── Inputs between windows ──────────────────────────────────────────────────────

	/// One received video frame: its RTP timestamp (90 kHz) and the local arrival time of
	/// its last packet (ms, any monotonic clock) — feeds the delay-gradient estimator.
	pub fn on_frame(&mut self, send_ts_90k: u32, arrival_ms: f64) {
		self.delay.on_frame(send_ts_90k, arrival_ms);
	}

	/// The host's current FEC group size as observed from parity packets (0 = none): the
	/// encoder gets `n/(n+1)` of the target so the wire rate stays at the target.
	pub fn set_fec_n(&mut self, n: u8) {
		self.fec_n = n;
	}

	/// The user pinned (or unpinned) the bitrate: resync the target without punishment.
	pub fn set_target(&mut self, kbps: u32) {
		self.target = kbps.clamp(FLOOR_KBPS, self.cap);
		self.since_step = 0;
		self.encoder_kbps = self.encoder_rate();
	}

	/// The user pinned a resolution/fps (`true`) or went back to automatic (`false`).
	pub fn set_manual_point(&mut self, manual: bool) {
		self.manual_point = manual;
		if !manual {
			self.point_idx = ladder::pick(&self.points, self.target, POINT_MARGIN);
			self.point_age = 0;
		}
	}

	/// A stream restart re-based the host's clocks: forget the delay history.
	pub fn on_stream_restart(&mut self) {
		self.delay.reset();
	}

	/// Fast reflex between windows (call every ≤ 200 ms with the same clock as
	/// `on_frame`): a sustained Overuse hypothesis cuts ×0.85 without waiting for the tick.
	pub fn poll_fast(&mut self, now_ms: f64) -> Option<Decision> {
		if self.delay.overuse_ms(now_ms) < FAST_OVERUSE_MS
			|| now_ms - self.last_fast_cut_ms < FAST_COOLDOWN_MS
			|| self.target <= FLOOR_KBPS
		{
			return None;
		}
		self.last_fast_cut_ms = now_ms;
		let before = self.target;
		self.punish(before);
		let mut d = Decision { reason: "delay overuse (fast) → ×0.85", ..Default::default() };
		self.apply_rate(before * 85 / 100, &mut d);
		self.apply_ladder(&mut d, true);
		Some(d)
	}

	// ── The window decision ─────────────────────────────────────────────────────────

	pub fn tick(&mut self, s: &Sample) -> Decision {
		self.windows += 1;
		self.since_step = self.since_step.saturating_add(1);
		self.point_age = self.point_age.saturating_add(1);
		if let Some(a) = self.probe_age.as_mut() {
			*a += 1;
		}
		let total = s.recv + s.lost;
		let loss = if total > 0 { s.lost as f32 / total as f32 } else { 0.0 };
		let (rtt, excess, jitter) = self.update_rtt(&s.rtt_ms);
		if loss < self.noise_loss {
			self.noise_loss = loss;
		}
		let eff = (loss - self.noise_loss).max(0.0);
		let delay = self.delay.state();
		self.last = Signals {
			loss,
			eff_loss: eff,
			rtt_ms: rtt,
			excess_ms: excess,
			jitter_ms: jitter,
			delay,
			trend: self.delay.trend() as f32,
		};
		let mut out = Decision::default();

		// Loss-recovery mode: flip once, on the first sustained loss.
		if total > 50 && loss > RECOVERY_LOSS {
			self.lossy += 1;
		} else {
			self.lossy = 0;
		}
		if self.recovery == LossRecovery::Normal
			&& total > 50
			&& (self.lossy >= RECOVERY_WINDOWS || loss > LOSS_DOWN)
		{
			self.recovery = self.preferred_recovery();
			out.recovery = Some(self.recovery);
			out.reason = "loss → recovery mode";
		}

		// A probe-down is being judged: did the loss follow the rate?
		if let Some(mut pd) = self.probe_down.take() {
			pd.windows += 1;
			pd.loss_acc += loss;
			if pd.windows < PROBE_DOWN_WINDOWS {
				self.probe_down = Some(pd);
				return self.finish(out);
			}
			let after = pd.loss_acc / pd.windows as f32;
			if pd.fec_n != self.fec_n {
				// FEC changed under the probe: inconclusive — restore, judge again later.
				self.apply_rate(pd.from, &mut out);
				out.reason = "probe down inconclusive (fec changed) — restored";
			} else if after <= pd.pre_loss * 0.6 {
				self.punish(pd.from);
				out.reason = "sustained loss followed the rate — kept the lower rate";
			} else {
				self.noise_loss = self.noise_loss.max(after);
				self.apply_rate(pd.from, &mut out);
				out.reason = "sustained loss did not follow the rate — learned as noise, restored";
			}
			return self.finish(out);
		}

		// "Now" excess: the window's LAST sample over the baseline. A queue that is draining
		// (after a cut) leaves the window mean elevated while the tail is already flat — that
		// must not read as a fresh queue.
		let excess_now = if self.rtt_base > 0.0 && self.rtt_last > 0.0 {
			(self.rtt_last - self.rtt_base).max(0.0)
		} else {
			0.0
		};
		let draining = delay == DelayState::Underuse || (excess >= RTT_OK_MS && excess_now < RTT_OK_MS);
		let queued = !draining && (excess >= RTT_OK_MS || delay == DelayState::Overuse);

		// Delay first: the gradient says a queue is filling.
		if delay == DelayState::Overuse && self.target > FLOOR_KBPS && self.since_step >= 1 {
			let before = self.target;
			self.punish(before);
			self.apply_rate(before * 85 / 100, &mut out);
			out.reason = "delay overuse → ×0.85";
			return self.finish(out);
		}
		// Keepalive RTT over its baseline (and still over it at the end of the window).
		if excess >= RTT_EXCESS_MS && !draining && self.target > FLOOR_KBPS && self.since_step >= 1 {
			let before = self.target;
			let cut = if excess >= RTT_BAD_MS { before / 2 } else { before * 7 / 10 };
			self.punish(before);
			self.apply_rate(cut, &mut out);
			out.reason = if excess >= RTT_BAD_MS { "rtt bloat severe → halve" } else { "rtt bloat → ×0.7" };
			return self.finish(out);
		}
		// Severe raw loss.
		if total > 100 && loss > LOSS_SEVERE && self.target > FLOOR_KBPS {
			let before = self.target;
			self.punish(before);
			self.apply_rate(before / 2, &mut out);
			out.reason = "severe loss → halve";
			return self.finish(out);
		}
		// Sustained mild loss.
		if total > 100 && eff > LOSS_DOWN {
			self.over += 1;
			self.clean = 0;
			self.at_ceiling = 0;
			self.clean_at_point = 0;
			if self.over >= 2 && self.since_step >= COOLDOWN_WINDOWS && self.target > FLOOR_KBPS {
				let before = self.target;
				if queued {
					self.punish(before);
					self.apply_rate(before * 7 / 10, &mut out);
					out.reason = "sustained loss with a queued link → ×0.7";
				} else {
					self.over = 0;
					self.probe_down = Some(ProbeDown { from: before, pre_loss: loss, windows: 0, loss_acc: 0.0, fec_n: self.fec_n });
					self.apply_rate(before * 4 / 5, &mut out);
					out.reason = "sustained loss, flat link → probe down";
				}
			}
			return self.finish(out);
		}
		// Clean window on an unqueued link: climb / probe.
		if total > 0 && eff < LOSS_CLEAN && !queued && delay != DelayState::Underuse {
			self.over = 0;
			self.deadband = 0;
			if self.probe_age.is_some_and(|a| a >= PROBE_PUNISH_WINDOWS) {
				self.probe_age = None;
				self.probe_wait = (self.probe_wait / 2).max(PROBE_WAIT_MIN);
			}
			self.clean_at_point += 1;
			self.clean_at_target += 1;
			if self.clean_at_target >= LAST_GOOD_WINDOWS {
				self.last_good = self.target;
			}
			if self.target < self.ceiling {
				self.clean += 1;
				self.at_ceiling = 0;
				let (need, factor) = if self.startup_left > 0 { (1, 1.5) } else { (CLEAN_WINDOWS_UP, 1.25) };
				if self.clean >= need {
					self.clean = 0;
					let next = ((self.target as f64 * factor) as u32).min(self.ceiling);
					self.apply_rate(next, &mut out);
					out.reason = if self.startup_left > 0 { "startup probe ×1.5" } else { "clean → climb toward ceiling" };
				}
			} else {
				self.clean = 0;
				self.at_ceiling += 1;
				if self.at_ceiling >= self.probe_wait && self.ceiling < self.cap {
					self.at_ceiling = 0;
					let next = (self.target as u64 * 11 / 10).min(self.cap as u64) as u32;
					self.ceiling = next.max(self.ceiling);
					self.probe_age = Some(0);
					self.apply_rate(next, &mut out);
					out.reason = "long clean stretch at ceiling → probe +10%";
				}
			}
			return self.finish(out);
		}
		// Deadband: a little loss, or the link slightly queued / draining → hold. Mild loss
		// that persists on a FLAT link is not congestion (the residual after FEC / NACK on a
		// noisy path): learn it as the noise floor so the climb can resume.
		self.over = 0;
		self.clean = 0;
		self.at_ceiling = 0;
		if total > 100 && eff > 0.0 && eff <= LOSS_DOWN && !queued && delay != DelayState::Underuse {
			self.deadband += 1;
			if self.deadband >= DEADBAND_LEARN_WINDOWS {
				self.deadband = 0;
				self.noise_loss = self.noise_loss.max(loss);
				out.reason = "steady mild loss on a flat link — learned as noise";
			}
		} else {
			self.deadband = 0;
		}
		self.finish(out)
	}

	// ── Internals ───────────────────────────────────────────────────────────────────

	fn finish(&mut self, mut out: Decision) -> Decision {
		self.startup_left = self.startup_left.saturating_sub(1);
		self.apply_ladder(&mut out, false);
		// The encoder share may change with FEC even when the target holds.
		let enc = self.encoder_rate();
		if enc != self.encoder_kbps {
			self.encoder_kbps = enc;
			if out.bitrate.is_none() {
				out.bitrate = Some(enc);
				if out.reason.is_empty() {
					out.reason = "fec share changed";
				}
			}
		}
		out
	}

	fn encoder_rate(&self) -> u32 {
		((self.target as f32 * fec_policy::encoder_share(self.fec_n)) as u32).max(FLOOR_KBPS.min(self.target))
	}

	fn update_rtt(&mut self, samples: &[f32]) -> (f32, f32, f32) {
		let mut jitter_acc = 0.0f32;
		let mut jitter_n = 0u32;
		let (mut sum, mut n) = (0.0f32, 0u32);
		for &x in samples {
			if x <= 0.0 {
				continue;
			}
			sum += x;
			n += 1;
			if self.rtt_base <= 0.0 || x < self.rtt_base {
				self.rtt_base = x;
			}
			if self.rtt_last > 0.0 {
				jitter_acc += (x - self.rtt_last).abs();
				jitter_n += 1;
			}
			self.rtt_last = x;
		}
		// The window's mean RTT (a cross-window EMA lagged a whole window after a queue
		// drained and turned the next mild-loss window into a false "congestion" cut).
		// With a single sample the previous value is blended in to damp one-off spikes.
		if n > 0 {
			let mean = sum / n as f32;
			self.rtt_ema = if self.rtt_ema <= 0.0 || n >= 2 { mean } else { (self.rtt_ema + mean) * 0.5 };
		}
		// Slow upward re-baseline (≈ 2 % of the gap per window): a path whose floor really
		// moved stops looking "bloated" after a few minutes.
		if self.rtt_base > 0.0 && self.rtt_ema > self.rtt_base {
			self.rtt_base += (self.rtt_ema - self.rtt_base) * 0.02;
		}
		let excess = if self.rtt_base > 0.0 && self.rtt_ema > 0.0 {
			(self.rtt_ema - self.rtt_base).max(0.0)
		} else {
			0.0
		};
		let jitter = if jitter_n > 0 { jitter_acc / jitter_n as f32 } else { 0.0 };
		(self.rtt_ema, excess, jitter)
	}

	/// A rate produced loss/bloat: remember it as a punished ceiling and, if it was a probe,
	/// double the wait before the next one.
	fn punish(&mut self, bad_rate: u32) {
		self.ceiling = (bad_rate as u64 * 85 / 100).clamp(FLOOR_KBPS as u64, self.cap as u64) as u32;
		if self.probe_age.is_some_and(|a| a <= PROBE_PUNISH_WINDOWS) {
			self.probe_wait = (self.probe_wait * 2).min(PROBE_WAIT_MAX);
		}
		self.probe_age = None;
		self.clean = 0;
		self.at_ceiling = 0;
		self.over = 0;
		self.clean_at_point = 0;
		self.startup_left = 0;
	}

	fn apply_rate(&mut self, kbps: u32, out: &mut Decision) {
		let kbps = kbps.clamp(FLOOR_KBPS, self.cap);
		self.since_step = 0;
		if kbps != self.target {
			self.clean_at_target = 0;
		}
		self.target = kbps;
		let enc = self.encoder_rate();
		if enc != self.encoder_kbps {
			self.encoder_kbps = enc;
			out.bitrate = Some(enc);
		}
	}

	/// Move along the ladder for the current target. `urgent` (a fast cut) ignores the
	/// debounce when the shortfall is severe.
	fn apply_ladder(&mut self, out: &mut Decision, urgent: bool) {
		if self.manual_point || self.points.len() < 2 {
			return;
		}
		let cur = self.points[self.point_idx];
		let target = self.target;
		// Down: the target no longer buys a clean picture at this point.
		if target < cur.min_kbps && self.point_idx + 1 < self.points.len() {
			let severe = (target as f32) < cur.min_kbps as f32 * 0.7;
			if self.point_age >= POINT_DEBOUNCE || severe || urgent {
				let want = ladder::pick(&self.points, target, POINT_MARGIN).max(self.point_idx + 1);
				self.point_idx = want.min(self.points.len() - 1);
				self.point_age = 0;
				self.clean_at_point = 0;
				out.point = Some(self.points[self.point_idx]);
				if out.reason.is_empty() {
					out.reason = "target under the point's minimum → step down";
				}
			}
			return;
		}
		// Up: after a clean stretch here (short during startup), to the best rung whose
		// minimum sits under `target / UP_HEADROOM` — several rungs at once when the budget
		// clearly allows it (a 20 Mbit path must not crawl up 20 s per rung).
		if self.point_idx > 0 {
			let need = if self.startup_left > 0 { 2 } else { CLEAN_WINDOWS_UP };
			let want = ladder::pick(&self.points, target, 1.0 / UP_HEADROOM);
			if want < self.point_idx && self.clean_at_point >= need && self.point_age >= POINT_DEBOUNCE {
				self.point_idx = want;
				self.point_age = 0;
				self.clean_at_point = 0;
				out.point = Some(self.points[self.point_idx]);
				if out.reason.is_empty() {
					out.reason = "clean with headroom → step up";
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cfg(cap: u32, ir: bool) -> Config {
		let mut c = Config::new(VCodec::H264, cap);
		c.ir_capable = ir;
		c
	}

	fn win(recv: u32, lost: u32, rtt: f32) -> Sample {
		Sample { recv, lost, rtt_ms: vec![rtt; 4], ..Default::default() }
	}

	/// A pipe with a hard cap: below it loss is 0 and RTT flat; above it, loss grows with
	/// the overshoot and a queue adds RTT.
	fn capped(rate: u32, cap: u32, base_rtt: f32) -> Sample {
		let pkts = rate / 8;
		if rate <= cap {
			win(pkts, 0, base_rtt)
		} else {
			let over = (rate - cap) as f32 / rate as f32;
			let lost = (pkts as f32 * over) as u32;
			win(pkts - lost, lost, base_rtt + 60.0 + over * 200.0)
		}
	}

	#[test]
	fn random_loss_with_flat_link_does_not_collapse_rate_or_point() {
		let mut c = Controller::new(cfg(8000, true));
		let mut probe_downs = 0;
		let mut flipped = None;
		for w in 0..150 {
			let d = c.tick(&win(965, 35, 120.0));
			if d.reason.contains("probe down") {
				probe_downs += 1;
			}
			if d.recovery.is_some() && flipped.is_none() {
				flipped = Some(w);
			}
		}
		assert!(c.target_kbps() >= 6400, "{}", c.target_kbps());
		assert_eq!(probe_downs, 1);
		assert!(c.noise_loss() >= 0.025);
		assert_eq!(c.recovery(), LossRecovery::IntraRefresh);
		assert!(flipped.unwrap() <= 1);
		assert_eq!(c.point().label(), "1080p30", "8000 kbit: 1080p60 needs 8000/0.85");
	}

	#[test]
	fn fixed_2mbit_cap_converges_to_a_clean_low_point_without_sawtooth() {
		let mut c = Controller::new(cfg(8000, false));
		let mut lossy_after_settle = 0;
		let mut point_changes_after_settle = 0;
		for w in 0..300 {
			let s = capped(c.encoder_kbps(), 1800, 20.0);
			if w >= 30 && s.lost > 0 {
				lossy_after_settle += 1;
			}
			let d = c.tick(&s);
			if w >= 30 && d.point.is_some() {
				point_changes_after_settle += 1;
			}
		}
		assert!(c.target_kbps() <= 1800 && c.target_kbps() >= 1000, "{}", c.target_kbps());
		assert!(lossy_after_settle <= 4, "{lossy_after_settle}");
		assert!(point_changes_after_settle <= 2, "{point_changes_after_settle}");
		let p = c.point();
		assert!(p.height <= 720 && p.fps == 30, "settles on a rung that is clean at ~1.5 Mbit: {}", p.label());
		assert!(c.probe_wait() >= PROBE_WAIT_MIN * 2);
		assert_eq!(c.recovery(), LossRecovery::ShortGop);
	}

	#[test]
	fn forced_20mbit_reaches_the_top_rung_within_30s_from_a_low_start() {
		let mut c = Config::new(VCodec::H264, 20_000);
		c.start_kbps = 3000;
		let mut c = Controller::new(c);
		assert_eq!(c.point().label(), "720p30");
		let mut top_at = None;
		for w in 0..40 {
			let d = c.tick(&win(2000, 0, 15.0));
			if let Some(p) = d.point {
				if p.label() == "1080p60" {
					top_at = Some(w);
					break;
				}
			}
		}
		assert!(matches!(top_at, Some(w) if w <= 15), "{top_at:?} target={}", c.target_kbps());
	}

	#[test]
	fn rtt_rise_cuts_before_loss_and_the_ladder_follows() {
		let mut c = Controller::new(cfg(12_000, true));
		for _ in 0..5 {
			c.tick(&win(1500, 0, 20.0));
		}
		assert_eq!(c.point().label(), "1080p60");
		let d = c.tick(&win(1500, 0, 160.0)); // severe bloat, zero loss
		assert_eq!(d.bitrate, Some(6000), "{d:?}");
		let d = c.tick(&win(1500, 0, 160.0));
		assert_eq!(d.bitrate, Some(3000), "{d:?}");
		assert_eq!(c.point().label(), "720p30", "6000 → 1080p30 → 3000 → 720p30");
	}

	#[test]
	fn delay_gradient_overuse_cuts_even_with_flat_rtt_and_zero_loss() {
		let mut c = Controller::new(cfg(8000, true));
		// Ideal frames, then a filling queue (each frame 1 ms later than the last).
		for i in 0..60 {
			c.on_frame((i as f64 * 16.667 * 90.0) as u32, i as f64 * 16.667 + 20.0);
		}
		assert_eq!(c.tick(&win(1000, 0, 20.0)).bitrate, None);
		for i in 60..130 {
			c.on_frame((i as f64 * 16.667 * 90.0) as u32, i as f64 * 16.667 + 20.0 + (i - 60) as f64);
		}
		assert_eq!(c.last().delay, DelayState::Normal, "state is sampled at the tick");
		let d = c.tick(&win(1000, 0, 20.0));
		assert_eq!(d.bitrate, Some(6800), "×0.85 on overuse: {d:?}");
		assert_eq!(c.last().delay, DelayState::Overuse);
	}

	#[test]
	fn fast_reflex_cuts_between_ticks_on_sustained_overuse() {
		let mut c = Controller::new(cfg(8000, true));
		for i in 0..60 {
			c.on_frame((i as f64 * 16.667 * 90.0) as u32, i as f64 * 16.667 + 20.0);
		}
		let mut cut = None;
		for i in 60..200 {
			let t = i as f64 * 16.667;
			c.on_frame((t * 90.0) as u32, t + 20.0 + (i - 60) as f64 * 1.5);
			if let Some(d) = c.poll_fast(t) {
				cut = Some((i, d));
				break;
			}
		}
		let (i, d) = cut.expect("a sustained overuse must trigger the fast cut");
		assert_eq!(d.bitrate, Some(6800), "{d:?}");
		// And not again within the cooldown.
		assert!(c.poll_fast(i as f64 * 16.667 + 100.0).is_none());
	}

	#[test]
	fn fec_share_is_deducted_from_the_encoder_rate() {
		let mut c = Controller::new(cfg(8000, true));
		assert_eq!(c.encoder_kbps(), 8000);
		c.set_fec_n(16);
		let d = c.tick(&win(1000, 0, 20.0));
		assert_eq!(d.bitrate, Some(7529), "8000 × 16/17: {d:?}");
		assert_eq!(c.target_kbps(), 8000, "the wire target is unchanged");
	}

	#[test]
	fn manual_point_pins_the_ladder_but_not_the_rate() {
		let mut m = cfg(8000, false);
		m.ladder = false;
		let mut c = Controller::new(m);
		let d = c.tick(&win(800, 200, 20.0));
		assert!(d.bitrate.is_some() && d.point.is_none(), "{d:?}");
		c.set_manual_point(false);
		assert_eq!(c.point().label(), "720p30", "unpinned: re-picked for the current target");
	}

	#[test]
	fn last_good_is_remembered_after_a_clean_half_minute() {
		let mut c = Controller::new(cfg(8000, false));
		assert_eq!(c.last_good_kbps(), 0);
		for _ in 0..LAST_GOOD_WINDOWS {
			c.tick(&win(1000, 0, 20.0));
		}
		assert_eq!(c.last_good_kbps(), 8000);
	}

	#[test]
	fn never_below_floor_and_never_above_cap() {
		let mut c = Controller::new(cfg(3000, false));
		for _ in 0..20 {
			c.tick(&win(500, 500, 300.0));
		}
		assert_eq!(c.target_kbps(), FLOOR_KBPS);
		assert_eq!(c.point().label(), "360p30");
		for _ in 0..2000 {
			c.tick(&win(1000, 0, 20.0));
		}
		assert!(c.target_kbps() <= 3000);
	}

	#[test]
	fn codec_ids_map_to_ladders() {
		assert_eq!(codec_from_wire("h265"), VCodec::H265);
		assert_eq!(codec_from_wire("HEVC"), VCodec::H265);
		assert_eq!(codec_from_wire("av1"), VCodec::Av1);
		assert_eq!(codec_from_wire("h264"), VCodec::H264);
		assert_eq!(codec_from_wire("anything"), VCodec::H264);
	}
}
