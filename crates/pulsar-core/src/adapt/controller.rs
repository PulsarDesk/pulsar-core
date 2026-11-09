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
//! * **No sawtooth.** A rate that produced CONFIRMED loss/bloat becomes a punished ceiling
//!   (×0.85). Confirmed = the keepalive RTT or loss backs the delay signal up, or the same
//!   kind of event repeats within `SOFT_REPEAT_WINDOWS`; a lone delay-only overuse or a lone
//!   loss burst on an unqueued link (a radio blip) cuts the rate but leaves the ceiling alone
//!   — the climb back stops just under the rate it happened at for `SOFT_REPEAT_WINDOWS`, so a
//!   real limit shows itself by repeating there instead of being re-hit at once.
//!   Climbing back to the ceiling is free (×1.25 per `CLIMB_WINDOWS` clean windows); going
//!   past it is a probe after `PROBE_WAIT_MIN` clean windows at the ceiling, whose step grows
//!   with every probe that survives (+10 % → +20 % → +40 % → +80 %) and drops back to +10 %
//!   on a failure, which also doubles the wait — a link that improved is climbed back in a
//!   few probes (5 → 15 Mbit ≈ 4 min), one that didn't sees rarer and rarer +10 % probes.
//!   (Was +10 % per minute for every probe, and a lone loss burst punished the ceiling: a
//!   mobile link that lost one burst at 12 Mbit then took ~10 minutes to get back.)
//! * **Startup fast.** For the first `startup_windows` a clean window climbs ×1.5 at once
//!   (and the ramp runs on past them while every window climbs cleanly, up to
//!   `STARTUP_MAX_WINDOWS`). A similar pace (×1.5 per 2 clean windows) applies whenever the
//!   target sits under half the ceiling — that gap is left by past cuts, not a probe past a
//!   known limit.
//! * **Host-bound queues.** Only the first queue-driven cut (delay overuse / RTT bloat) of a
//!   run lowers the ceiling. When the queue persists through `HOST_BOUND_CUTS` cuts in a
//!   row the bitrate is not what fills it — the host can't encode/send this point in real
//!   time (a software encoder at 1080p120): step to a lighter point (fewer pixels × fps)
//!   instead of starving the encoder, and don't climb back above it until a long clean
//!   stretch (`POINT_RETRY_WINDOWS`, doubling on each repeat).
//!
//! Ladder rules: pick the highest point whose minimum fits `target × 0.85`; go **down** as
//! soon as the target is under the current point's minimum (resolution changes restart an
//! encoder, so at most one change per 2 windows unless the shortfall is severe); go **up**
//! one rung only after 20 s clean at the current point and `target ≥ next.min × 1.2`.
//!
//! Poor-link rules (2026-11-09 — a 10 Mbit tethered mobile link at 78 ms collapsed a saved
//! 1080p120 / 15 Mbit pick to 300 kbit/s, 0–3 fps, for minutes):
//!
//! * **Cautious start.** No per-peer memory starts at `START_KBPS_NO_MEMORY` (≤ the cap),
//!   not the cap; the startup ramp climbs ×1.5 per clean window (was ×2) for as long as it
//!   stays clean, and settles at the first congestion sign (delay overuse, loss over noise).
//! * **Point-relative floor.** The rate never sits under the current point's minimum (a
//!   1080p120 encoder at 300 kbit/s is ~300 bytes a frame: unwatchable AND it overshoots).
//!   A rate the point can't hold steps the POINT down instead; only the bottom rung may go
//!   lower, down to an fps-aware floor (`KBPS_PER_FPS_FLOOR`).
//! * **Picks are soft.** A pinned resolution / fps (saved pick, session menu) is the
//!   preferred MAXIMUM: the ladder under it steps the fps down first, then the resolution
//!   (never under the session's own floors), and climbs back toward it, never past it
//!   (`pick_shortfall` tells the app when it is below the pick).
//! * **Loss is measured, not guessed.** With `Sample::recv_bytes` / `window_ms` the window's
//!   goodput is known: a loss cut goes to `0.85 × delivered` (GCC style) instead of a blind
//!   halving, at most one cut per window (the fast reflex counts), and gets a window to show
//!   its effect. A cut that lowered what the host sent without lowering the loss proves the
//!   loss non-congestive: it is learned as noise and the cut undone (`loss_is_non_responsive`
//!   — FEC's job, not the rate's). A host that sends far more than asked (keyframes, parity)
//!   steps the point down instead of starving the target further.
//! * **Never stuck at the floor.** `FLOOR_ESCAPE_WINDOWS` at the floor with steady loss probe
//!   up, whatever the punished ceiling says.
//! * **A policer is learned, not re-hit** (2026-11-09 "pol10": 1040 pkt/s policer, 1 % random
//!   loss, 78 ms — the rate went back to the one that had just failed every ~10 s, each time
//!   1–2 s of near-frozen video). A measured window judges congestion on its PRE-repair loss
//!   (`raw_loss`), which the parity share doesn't move, so a probe-down that cured it is no
//!   longer "inconclusive (fec changed)". A cut that cures it is a loss episode at that rate:
//!   the ceiling goes to `EPISODE_CEILING_PCT` of it (or the clean rate just under it), the
//!   climb back and the next probes approach it in `NEAR_BAD_STEP_PCT` steps, loss right after
//!   a climb / probe step goes straight back to the rate before it (one lossy window, not
//!   three), and each failed probe doubles the wait (1, 2, 4 … min). Steady mild pre-repair
//!   loss is tested with a probe-down before it is learned as noise: a policer's overflow is
//!   steady and flat too. Unmeasured samples (mobile) keep the post-repair rules.

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

/// Consecutive clean windows at a point before the ladder steps up (20 s) — resolution /
/// fps changes restart the encoder, so they stay unhurried.
pub const CLEAN_WINDOWS_UP: u32 = 10;
/// Consecutive clean windows before a ×1.25 climb toward the (known-good) ceiling (8 s).
pub const CLIMB_WINDOWS: u32 = 4;
/// Clean windows at the ceiling before a probe past it (60 s; doubles per failed probe).
pub const PROBE_WAIT_MIN: u32 = 30;
/// The first probe's step past the ceiling (%); each surviving probe doubles it up to
/// `PROBE_STEP_MAX_PCT`, a failed one resets it.
pub const PROBE_STEP_MIN_PCT: u32 = 10;
pub const PROBE_STEP_MAX_PCT: u32 = 80;
/// An unconfirmed cut (delay-only overuse, a lone loss burst) that repeats within this many
/// windows (60 s) is confirmed after all — the link really is at its limit there.
pub const SOFT_REPEAT_WINDOWS: u32 = 30;
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
/// Queue-driven cuts in a row (no calm window between) after which the queue is judged
/// host-side: the ladder steps to a lighter point instead of cutting the rate again.
pub const HOST_BOUND_CUTS: u32 = 3;
/// Clean windows at a host-bound point before the heavier one is tried again (5 min).
pub const POINT_RETRY_WINDOWS: u32 = 150;
/// Upper bound of the doubling point-retry wait (40 min).
pub const POINT_RETRY_MAX: u32 = 1200;
/// Under this share of the ceiling the climb runs at the startup pace.
pub const FAST_CLIMB_SHARE: f32 = 0.5;
/// Starting rate (kbit/s) without a per-peer memory (`Config::start_kbps` 0), capped by the
/// cap. Was the cap itself: a 15 Mbit pick opened a 10 Mbit mobile link at 15 Mbit and the
/// first seconds were 35–40 % loss.
pub const START_KBPS_NO_MEMORY: u32 = 4_000;
/// Startup ramp: ×1.5 per clean window (was ×2 — one step past a 10 Mbit link from 6 Mbit
/// overshot it by 20 %, the ×1.5 step by 0 %).
pub const STARTUP_FACTOR: f64 = 1.5;
/// The startup ramp keeps going past `Config::startup_windows` while every window climbs
/// cleanly, up to this many windows (a clean 100 Mbit LAN reaches the cap in ~16 s).
pub const STARTUP_MAX_WINDOWS: u32 = 15;
/// fps-aware rate floor (kbit/s per fps ≈ 1.5 KB a frame): under it the encoder's per-frame
/// budget is too small to hold (NVENC overshoots a 300-byte frame budget many times over).
pub const KBPS_PER_FPS_FLOOR: u32 = 12;
/// A congestion cut with measured goodput: the new target is this share of the rate the
/// path actually delivered (GCC's β).
pub const GOODPUT_BETA: f32 = 0.85;
/// The host's estimated send rate over the encoder rate asked for, above which the host is
/// not honouring the rate (keyframes, parity): cutting the target further doesn't help.
pub const HOST_OVERSHOOT: f32 = 1.6;
/// Non-responsive loss is judged after a cut that lowered the host's send rate under this
/// share of what it was (see `severe_loss_measured`).
pub const NON_RESPONSIVE_RATE_DROP: f32 = 0.8;
/// Windows after a loss cut during which its effect is judged (the first one only shows it
/// half: the re-request takes an RTT and the window was already running).
pub const LOSS_JUDGE_WINDOWS: u32 = 4;
/// Consecutive windows the wanted rate must sit under the point's minimum before a LOSS
/// shortfall steps the point down (a single bad window doesn't; a severe one does).
pub const POINT_SHORT_WINDOWS: u32 = 2;
/// Windows at the floor with steady loss after which the controller probes up anyway (20 s).
pub const FLOOR_ESCAPE_WINDOWS: u32 = 10;
/// A loss episode at a rate (measured path: raw loss over the noise that a cut cured) puts
/// the ceiling at this share of it (%): the limit sits just under where it failed, not a
/// whole ×0.85 under, and not AT it (the climb would walk straight back into it).
pub const EPISODE_CEILING_PCT: u32 = 90;
/// Climb / probe step (%) while a loss episode's rate is the known-bad level: the approach to
/// a level that just failed goes in small steps (was ×1.25 per climb, +10 → +80 % per probe).
pub const NEAR_BAD_STEP_PCT: u32 = 8;
/// A rate step (climb or probe) is the suspect for loss over the noise this many windows
/// after it: the rate goes straight back to the one before it (measured path).
pub const RAISE_SUSPECT_WINDOWS: u32 = 2;

/// The fps-aware rate floor for a point running at `fps` (see `KBPS_PER_FPS_FLOOR`).
pub fn rate_floor_for_fps(fps: u32) -> u32 {
	FLOOR_KBPS.max(fps.saturating_mul(KBPS_PER_FPS_FLOOR))
}

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
	/// UDP payload bytes of the video RTP packets received in the window — first copies
	/// only (NACK-repaired ones included, duplicates not), parity not counted. With
	/// `window_ms` it gives the window's goodput.
	pub recv_bytes: u64,
	/// The window's length (ms); 0 = unknown → no goodput logic (the pre-2026-11-09 paths).
	/// Set, the controller also judges congestion on the window's PRE-repair loss,
	/// `(lost + nack_ok + fec_ok) / (recv + lost)`: what the path dropped, whatever the
	/// parity / NACK got back afterwards.
	pub window_ms: u32,
}

/// The window's pre-repair (raw) loss when the sample is a measured one (`window_ms` set),
/// else the post-repair loss `post`. Post-repair loss moves with the parity share: on the
/// 2026-11-09 pol10 run the host's FEC went 15 → 50 % on every loss episode, so a probe-down
/// that cured the congestion read "inconclusive" and was undone, ten seconds a cycle.
fn raw_loss(s: &Sample, post: f32) -> f32 {
	let total = s.recv.saturating_add(s.lost);
	if s.window_ms == 0 || total == 0 {
		return post;
	}
	let dropped = s.lost.saturating_add(s.nack_ok).saturating_add(s.fec_ok);
	(dropped as f32 / total as f32).clamp(post, 1.0)
}

/// Derived per-window signals (logs, HUD, client→host stats).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Signals {
	/// Post-repair loss (what is still missing at the window's end).
	pub loss: f32,
	/// The loss the rate logic judges (pre-repair on a measured window, else `loss`) over the
	/// learned noise floor.
	pub eff_loss: f32,
	/// Pre-repair loss (= `loss` without `Sample::window_ms`).
	pub raw_loss: f32,
	pub rtt_ms: f32,
	pub excess_ms: f32,
	pub jitter_ms: f32,
	pub delay: DelayState,
	/// Gain-scaled delay trend (ms/ms × gain) — for logs.
	pub trend: f32,
	/// Video goodput of the window (kbit/s; 0 = not measured).
	pub goodput_kbps: f32,
	/// The host's estimated video send rate (goodput scaled by the loss; 0 = not measured).
	pub send_kbps: f32,
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
	/// Starting target (kbit/s); 0 = no memory → `START_KBPS_NO_MEMORY` (at most the cap).
	/// Seed with 60 % of the peer's last good rate.
	pub start_kbps: u32,
	/// Whether this client can resume on a partially intra-refreshed picture.
	pub ir_capable: bool,
	/// Run the resolution/fps ladder. **Off by default** (maintainer decision, 2026-09-03):
	/// the resolution and fps are fixed at session start (settings / what the display can
	/// take) and only the bitrate adapts within them. `true` = the automatic ladder.
	pub ladder: bool,
	/// Windows of aggressive startup probing (a window that climbs cleanly doesn't use one
	/// up, see `STARTUP_MAX_WINDOWS`; the first congestion sign ends the ramp).
	pub startup_windows: u32,
	/// Ladder floor height: rungs below `min(min_height, native height)` are dropped, so the
	/// ladder only trades fps (and the bitrate inside a rung) for bandwidth, never resolution.
	/// 0 = no floor. Remote-desktop sessions set 1080: blurry text at 540p is worse than a
	/// lower frame rate.
	pub min_height: u32,
	/// Ladder floor frame rate, the game-mode twin of `min_height`: rungs below
	/// `min(min_fps, top fps)` are dropped (except the cheapest rung as a last resort, when no
	/// `min_height` is set) and
	/// 60 fps rungs are added at the low heights, so a thin path trades resolution, not
	/// smoothness. 0 = no floor. Game sessions set 60: 720p30 on a 3 Mbit mobile path felt
	/// far worse than 540p60 would.
	pub min_fps: u32,
	/// Scale for every rung's minimum bitrate (1.0 = the game-content reference table).
	/// Desktop content is mostly static and needs far fewer bits for the same look.
	pub min_kbps_scale: f32,
}

impl Config {
	pub fn new(codec: VCodec, cap_kbps: u32) -> Self {
		Self {
			codec,
			native: (0, 0, 0),
			cap_kbps,
			start_kbps: 0,
			ir_capable: false,
			ladder: false,
			startup_windows: 5,
			min_height: 0,
			min_fps: 0,
			min_kbps_scale: 1.0,
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
	/// The rate held while the probe is judged (×0.8 of `from`, or the rate before the step
	/// that drew the loss).
	to: u32,
	pre_loss: f32,
	windows: u32,
	loss_acc: f32,
	/// FEC overhead when the probe started: parity switching on/up mid-probe lowers the
	/// post-repair loss by itself, so an unmeasured probe's judgement is only valid when it
	/// stayed the same. A measured one judges the pre-repair loss, which parity doesn't move.
	fec_overhead: f32,
	measured: bool,
	/// The learned noise floor when the probe started. The judgement compares loss OVER it:
	/// raw loss can never fall to 60 % of a window that sat on a 35 % random floor, so every
	/// probe read "did not follow" and the rate looped 11 → 9 → 11 Mbit over a 10 Mbit policer.
	noise: f32,
}

/// A loss cut made with measured goodput, kept to judge whether the loss followed it.
#[derive(Clone, Copy, Debug)]
struct LossCut {
	/// Window of the (latest) cut.
	window: u32,
	/// The host's estimated video send rate and the raw loss in that window.
	send_before: f32,
	loss_before: f32,
	/// Rate and ceiling before the FIRST cut of the run (what a non-responsive verdict restores),
	/// and the known-bad level then (`Controller::bad_rate`).
	from: u32,
	from_ceiling: u32,
	from_bad: u32,
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
	/// Step of the next probe past the ceiling (%).
	probe_step_pct: u32,
	/// Window of the last unconfirmed queue cut / lone loss burst (see `SOFT_REPEAT_WINDOWS`).
	soft_cut_at: Option<u32>,
	soft_loss_at: Option<u32>,
	/// After an unconfirmed cut: the climb stays under this rate until that window.
	soft_limit: Option<(u32, u32)>,
	/// The transport's own cap (relay / session request) and the user's bandwidth limit
	/// (0 = none): `cap` is the smaller of the two.
	transport_cap: u32,
	user_cap: u32,
	since_step: u32,
	over: u32,
	lossy: u32,
	noise_loss: f32,
	/// The last rate a window at or under the (large) noise floor was seen at.
	noise_ok_rate: u32,
	/// Mean deviation of the windows' loss around a large noise floor (see `loss_tolerance`).
	noise_dev: f32,
	probe_down: Option<ProbeDown>,
	rtt_base: f32,
	rtt_ema: f32,
	rtt_last: f32,
	/// The window's RTT floor (minimum sample) — what the bloat checks compare to the base.
	rtt_floor: f32,
	/// The previous window already read as queued (floor excess ≥ `RTT_OK_MS`).
	queued_prev: bool,
	/// Consecutive deadband windows (mild loss, flat link), and their summed judged loss.
	deadband: u32,
	deadband_acc: f32,
	recovery: LossRecovery,
	startup_left: u32,
	/// The startup ramp already paused once on a mild-loss window (the next one ends it).
	startup_paused: bool,
	windows: u32,
	last: Signals,
	// Ladder.
	points: Vec<Point>,
	/// The user's pinned resolution / fps (session menu): the ladder keeps adapting the
	/// OTHER dimension (and the rate) within them — see `set_pins`.
	pin_res: Option<(u32, u32)>,
	pin_fps: Option<u32>,
	point_idx: usize,
	point_age: u32,
	clean_at_point: u32,
	manual_point: bool,
	/// Queue-driven cuts since the last calm window.
	queue_cuts: u32,
	/// The heaviest point the host proved it can keep up with (index; 0 = no limit).
	point_cap: usize,
	/// Clean windows spent at `point_cap`, and the wait before retrying the next heavier one.
	at_point_cap: u32,
	point_retry: u32,
	/// Windows left in which the host's queue drains after a host-bound step (no queue cuts).
	drain_left: u32,
	/// The encoder bitrate last decided (net of FEC).
	encoder_kbps: u32,
	/// FEC parity overhead currently observed on the wire (parity bytes / media bytes).
	fec_overhead: f32,
	// Delay estimator + fast reflex.
	delay: Trendline,
	last_fast_cut_ms: f64,
	// Memory.
	clean_at_target: u32,
	last_good: u32,
	/// Startup-settle bookkeeping (see `startup_settled`): the target at the previous window's
	/// end, and the window of the last rate cut / operating-point change.
	settle_prev_target: u32,
	last_cut_window: u32,
	last_point_window: u32,
	/// Window of the last change of ANY kind (rate up or down, or point) and the number of
	/// windows that actually carried packets (a timer's empty first tick doesn't count).
	last_change_window: u32,
	data_windows: u32,
	/// The rate the rate logic wants (kbit/s) before the point floor: `target` is this, but
	/// never under the current point's minimum (`point_floor`). Under it = the point is too
	/// heavy for the path.
	want: u32,
	/// Consecutive windows `want` sat under the current point's minimum.
	point_short: u32,
	/// A queue-driven cut (delay / RTT) happened this window: the ladder may step at once
	/// (debounced), like before — a filling queue can't wait for the 2-window loss rule.
	queue_cut_now: bool,
	/// The fast reflex already cut since the last tick: that was this window's cut.
	fast_cut_pending: bool,
	/// The encoder rate in effect at the end of the previous window (what the host was asked
	/// to send during this one, before any fast cut).
	win_enc_ref: u32,
	/// The last window's measured goodput / estimated host send rate (kbit/s; video).
	goodput: Option<f32>,
	send_est: Option<f32>,
	loss_cut: Option<LossCut>,
	/// The last loss judgement found the loss non-congestive (see `loss_is_non_responsive`).
	non_responsive: bool,
	/// The startup ramp climbed this window (it doesn't use up a startup window).
	startup_climbed: bool,
	/// Windows spent at the floor with steady loss, and that loss (see `FLOOR_ESCAPE_WINDOWS`).
	floor_windows: u32,
	floor_loss: f32,
	/// Floor escapes since the last probe that survived (the next escape waits twice as long).
	floor_escapes: u32,
	/// The current window carried `Sample::window_ms`: the loss judged is the pre-repair one
	/// and the loss-episode rules below are on (off = the pre-2026-11-09 paths, mobile).
	measured: bool,
	/// The rate of the last confirmed loss episode (0 = none), until a probe past it survives:
	/// climbs and probes approach it in `NEAR_BAD_STEP_PCT` steps.
	bad_rate: u32,
	/// The last climb / probe step: the rate before it and its window (see
	/// `RAISE_SUSPECT_WINDOWS`).
	raise: Option<(u32, u32)>,
	/// The ceiling before the current probe past it (held clean for the whole probe wait).
	probe_from: u32,
}

/// `Config::min_fps`: keep the frame rate, give up resolution. Adds `keep`-fps rungs at the
/// heights the reference ladder only offers at 30 fps (540p, 360p), drops every rung under
/// `keep` except the cheapest overall, and restores `ladder::build`'s order.
fn apply_fps_floor(points: &mut Vec<Point>, cfg: &Config, scale: &dyn Fn(Point) -> Point) {
	let top_fps = points.iter().map(|p| p.fps).max().unwrap_or(0);
	let keep = cfg.min_fps.min(top_fps);
	let Some(top) = points.first().copied() else { return };
	let aspect = top.width as f64 / top.height as f64;
	for h in [540u32, 360] {
		// Never below the resolution floor (`min_height`), when one is set.
		if h < cfg.min_height {
			continue;
		}
		if h < top.height && !points.iter().any(|p| p.height == h && p.fps == keep) {
			let w = (((h as f64 * aspect / 16.0).round() as u32) * 16).max(160);
			points.push(scale(ladder::point_at(cfg.codec, w, h, keep)));
		}
	}
	// Without a resolution floor the cheapest rung stays as a last resort. With one, the
	// lowest `keep`-fps rung at the floor IS the bottom: a budget under its minimum lowers
	// the bitrate inside it (a blockier picture, still smooth) instead of halving the fps.
	let cheapest = points.iter().copied().min_by_key(|p| p.min_kbps).filter(|_| cfg.min_height == 0);
	points.retain(|p| p.fps >= keep || Some(*p) == cheapest);
	points.sort_by(|a, b| {
		let ka = a.width as u64 * a.height as u64 * a.fps as u64;
		let kb = b.width as u64 * b.height as u64 * b.fps as u64;
		b.min_kbps.cmp(&a.min_kbps).then(kb.cmp(&ka))
	});
	points.dedup();
}

/// The soft-pinned ladder (see `build_points`): the pick on top, then the chain under it,
/// built from `full` (the session's floored ladder) where it has the rung, otherwise from
/// `ladder::point_at` (a display mode that isn't a rung gets its own rungs).
fn pinned_chain(
	cfg: &Config,
	full: &[Point],
	pin_res: Option<(u32, u32)>,
	pin_fps: Option<u32>,
	scale: &dyn Fn(Point) -> Point,
) -> Vec<Point> {
	let Some(top) = full.first().copied() else { return Vec::new() };
	let (rw, rh) = pin_res.unwrap_or((top.width, top.height));
	let mut fps: Vec<u32> = full.iter().map(|p| p.fps).chain(pin_fps).collect();
	fps.sort_unstable_by(|a, b| b.cmp(a));
	fps.dedup();
	let f_max = pin_fps.unwrap_or(fps[0]);
	// The fps floor: the session's (game mode: 60), or the ladder's lowest rate; a pick
	// already under it keeps itself.
	let f_floor = if cfg.min_fps > 0 { cfg.min_fps } else { fps.last().copied().unwrap_or(f_max) }.min(f_max);
	let fps_steps: Vec<u32> = fps.iter().copied().filter(|&f| f <= f_max && f >= f_floor).collect();
	let h_floor = if cfg.min_height > 0 { cfg.min_height.min(rh) } else { 0 };
	let mut heights: Vec<u32> = full.iter().map(|p| p.height).filter(|&h| h < rh && h >= h_floor).collect();
	heights.sort_unstable_by(|a, b| b.cmp(a));
	heights.dedup();
	let aspect = rw as f64 / rh.max(1) as f64;
	let width_at = |h: u32| if h == rh { rw } else { (((h as f64 * aspect / 16.0).round() as u32) * 16).max(160) };
	let at = |w: u32, h: u32, f: u32| {
		full.iter()
			.copied()
			.find(|p| p.width == w && p.height == h && p.fps == f)
			.unwrap_or_else(|| scale(ladder::point_at(cfg.codec, w, h, f)))
	};
	let mut out = Vec::new();
	if pin_res.is_some() {
		// A pinned size (alone or with an fps): the fps gives way first, then the size.
		for &f in &fps_steps {
			out.push(at(rw, rh, f));
		}
		let f = fps_steps.last().copied().unwrap_or(f_max);
		for &h in &heights {
			out.push(at(width_at(h), h, f));
		}
	} else {
		// Only the fps pinned: the size gives way first, then the fps at the lowest size.
		out.push(at(rw, rh, f_max));
		for &h in &heights {
			out.push(at(width_at(h), h, f_max));
		}
		let h = heights.last().copied().unwrap_or(rh);
		for &f in fps_steps.iter().skip(1) {
			out.push(at(width_at(h), h, f));
		}
	}
	out
}

/// The session's ladder: the codec's rungs for the native size, minimums scaled for the
/// content (`min_kbps_scale`), the resolution floor (`min_height`), then capped by the
/// user's pins. Never empty.
///
/// Pins are the preferred MAXIMUM, not a single rung (2026-11-09: a saved 1080p120 pick on a
/// 10 Mbit mobile link sat on its one rung — 8.4 Mbit minimum — while the rate fell to
/// 300 kbit/s). The pinned point is the top rung; under it the ladder trades the dimension
/// the user did NOT pin first: with a pinned size (or both pinned) the fps steps down to the
/// session's fps floor, then the size steps down at that fps; with only a pinned fps the size
/// steps down first, then the fps. Never under the session's floors (`min_height`,
/// `min_fps`), except that a pick already under a floor keeps itself.
fn build_points(cfg: &Config, pin_res: Option<(u32, u32)>, pin_fps: Option<u32>) -> Vec<Point> {
	let scale = |mut p: Point| {
		if cfg.min_kbps_scale > 0.0 && (cfg.min_kbps_scale - 1.0).abs() > f32::EPSILON {
			p.min_kbps = ((p.min_kbps as f32 * cfg.min_kbps_scale).round() as u32).max(1);
		}
		p
	};
	let mut points: Vec<Point> = ladder::build(cfg.codec, cfg.native.0, cfg.native.1, cfg.native.2)
		.into_iter()
		.map(scale)
		.collect();
	if cfg.min_height > 0 {
		let top_h = points.first().map(|p| p.height).unwrap_or(0);
		let keep = cfg.min_height.min(top_h);
		points.retain(|p| p.height >= keep);
	}
	if cfg.min_fps > 0 {
		apply_fps_floor(&mut points, cfg, &scale);
	}
	if pin_res.is_some() || pin_fps.is_some() {
		points = pinned_chain(cfg, &points, pin_res, pin_fps, &scale);
	}
	points.sort_by(|a, b| {
		let ka = a.width as u64 * a.height as u64 * a.fps as u64;
		let kb = b.width as u64 * b.height as u64 * b.fps as u64;
		b.min_kbps.cmp(&a.min_kbps).then(kb.cmp(&ka))
	});
	points.dedup();
	if points.is_empty() {
		points.push(scale(ladder::point_at(cfg.codec, cfg.native.0.max(640), cfg.native.1.max(360), cfg.native.2.max(30))));
	}
	points
}

impl Controller {
	pub fn new(cfg: Config) -> Self {
		let cap = cfg.cap_kbps.max(FLOOR_KBPS);
		// No memory: a cautious start under the cap, the startup ramp finds the rest (a
		// clean LAN is at a 20 Mbit cap in 4 windows, a mobile link isn't opened at 15 Mbit).
		let start = if cfg.start_kbps == 0 {
			cap.min(START_KBPS_NO_MEMORY)
		} else {
			cfg.start_kbps.clamp(FLOOR_KBPS, cap)
		};
		let points = build_points(&cfg, None, None);
		let point_idx = if cfg.ladder { ladder::pick(&points, start, POINT_MARGIN) } else { 0 };
		let mut c = Self {
			target: start,
			cap,
			ceiling: cap,
			clean: 0,
			at_ceiling: 0,
			probe_wait: PROBE_WAIT_MIN,
			probe_step_pct: PROBE_STEP_MIN_PCT,
			soft_cut_at: None,
			soft_loss_at: None,
			soft_limit: None,
			transport_cap: cap,
			user_cap: 0,
			probe_age: None,
			since_step: COOLDOWN_WINDOWS,
			over: 0,
			lossy: 0,
			noise_loss: 0.0,
			noise_ok_rate: 0,
			noise_dev: 0.0,
			probe_down: None,
			rtt_base: 0.0,
			rtt_ema: 0.0,
			rtt_last: 0.0,
			rtt_floor: 0.0,
			queued_prev: false,
			deadband: 0,
			deadband_acc: 0.0,
			recovery: LossRecovery::Normal,
			startup_left: cfg.startup_windows,
			startup_paused: false,
			windows: 0,
			last: Signals::default(),
			points,
			pin_res: None,
			pin_fps: None,
			point_idx,
			point_age: POINT_DEBOUNCE,
			clean_at_point: 0,
			manual_point: !cfg.ladder,
			queue_cuts: 0,
			point_cap: 0,
			at_point_cap: 0,
			point_retry: 0,
			drain_left: 0,
			encoder_kbps: 0,
			fec_overhead: 0.0,
			delay: Trendline::new(),
			last_fast_cut_ms: -1.0e9,
			clean_at_target: 0,
			last_good: 0,
			settle_prev_target: start,
			last_cut_window: 0,
			last_point_window: 0,
			last_change_window: 0,
			data_windows: 0,
			want: start,
			point_short: 0,
			queue_cut_now: false,
			fast_cut_pending: false,
			win_enc_ref: 0,
			goodput: None,
			send_est: None,
			loss_cut: None,
			non_responsive: false,
			startup_climbed: false,
			floor_windows: 0,
			floor_loss: 0.0,
			floor_escapes: 0,
			measured: false,
			bad_rate: 0,
			raise: None,
			probe_from: 0,
			cfg,
		};
		c.retarget();
		c.encoder_kbps = c.encoder_rate();
		c.win_enc_ref = c.encoder_kbps;
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
	/// The FEC overhead the encoder rate is currently net of (0 = no parity).
	pub fn fec_overhead(&self) -> f32 {
		self.fec_overhead
	}
	/// The most recent rate that stayed clean for `LAST_GOOD_WINDOWS` (0 = none yet). The
	/// app persists it per peer and seeds the next session with 60 % of it.
	/// The opening phase has found a stable operating point: at least two windows in, no rate
	/// cut for the last two windows, no resolution/fps change in the last one, and the last
	/// window clean. The app keeps the "preparing" screen up until this (or a timeout), so the
	/// first picture the user sees is already at the rate the path holds — the climb / back-off
	/// turbulence of the first seconds happens behind it. Adaptation continues afterwards.
	pub fn startup_settled(&self) -> bool {
		self.data_windows >= 2
			&& self.windows.saturating_sub(self.last_cut_window) >= 2
			&& self.windows.saturating_sub(self.last_point_window) >= 1
			// Still climbing (the startup ramp raises the rate every window) is not settled
			// either: the first picture would come at a rate that is about to change.
			&& self.windows.saturating_sub(self.last_change_window) >= 1
			&& self.last.eff_loss < LOSS_DOWN
	}

	pub fn last_good_kbps(&self) -> u32 {
		self.last_good
	}

	/// While the user pinned a resolution and/or fps: `(picked, current)` when the current
	/// point is lighter than the pick because the path (or the host / decoder) can't hold it;
	/// `None` otherwise — no pins, at the pick, or still in the startup climb toward it. The
	/// app tells the user once when this turns `Some` (or the current point changes).
	pub fn pick_shortfall(&self) -> Option<(Point, Point)> {
		if (self.pin_res.is_none() && self.pin_fps.is_none()) || self.manual_point || self.point_idx == 0 {
			return None;
		}
		// The startup climb toward the pick isn't a shortfall yet. Afterwards it is, even in
		// a window whose rate would already buy the pick: the step up is debounced (and held
		// back during a probe), so that answer would only flicker.
		if self.startup_left > 0 {
			return None;
		}
		Some((self.points[0], self.point()))
	}

	/// The last loss judgement found the loss non-congestive: a rate cut lowered what the
	/// host sent but not the loss (random radio loss, a policer on bursts, MTU trouble). The
	/// controller stopped cutting on it — recovering it is FEC's / NACK's job. Clears once the
	/// loss goes away.
	pub fn loss_is_non_responsive(&self) -> bool {
		self.non_responsive
	}

	/// The lowest rate the current point is run at (kbit/s): its minimum, or on the bottom
	/// rung (or with the ladder off) the fps-aware floor.
	pub fn rate_floor_kbps(&self) -> u32 {
		self.point_floor()
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

	/// The parity overhead observed on the wire (Reed-Solomon `m/k`, or `1/n` for the XOR
	/// groups): the encoder gets `1/(1+overhead)` of the target so the wire rate stays at
	/// the target.
	pub fn set_fec_overhead(&mut self, overhead: f32) {
		self.fec_overhead = overhead.max(0.0);
	}

	/// XOR-group form of [`set_fec_overhead`](Self::set_fec_overhead) (`n` = 0 → off).
	pub fn set_fec_n(&mut self, n: u8) {
		self.set_fec_overhead(if n == 0 { 0.0 } else { 1.0 / n as f32 });
	}

	/// The user's bandwidth limit (kbit/s; 0 = none): a CEILING for the adaptive rate, not a
	/// fixed rate — the controller keeps adapting under it (a 10 Mbit limit on a 50 Mbit line
	/// still backs off when the line congests) and picks the best point the limit buys.
	/// Lowering it clamps the target at once; raising it lifts a ceiling that was only the old
	/// limit (a ceiling learned from real loss below it stays).
	pub fn set_user_cap(&mut self, kbps: u32) {
		self.user_cap = kbps;
		let old_cap = self.cap;
		let new_cap = if kbps > 0 { kbps.min(self.transport_cap) } else { self.transport_cap }.max(FLOOR_KBPS);
		self.cap = new_cap;
		if self.ceiling >= old_cap || self.ceiling > new_cap {
			self.ceiling = new_cap;
		}
		self.want = self.want.min(new_cap);
		self.since_step = 0;
		if !self.manual_point && self.cfg.ladder {
			self.point_idx = ladder::pick(&self.points, self.want, POINT_MARGIN).max(self.point_cap);
			self.point_age = 0;
		}
		self.retarget();
		self.encoder_kbps = self.encoder_rate();
	}

	/// The network path changed under the stream — a relayed session upgraded to a direct
	/// path mid-session. Everything learned about the old path (its ceiling and punishments,
	/// the RTT base, the delay trend, probes, soft limits, and a "host can't keep up" point
	/// cap that was really the relay's queue) says nothing about the new one: start over on
	/// it like a fresh session — the startup climb from the current rate — under
	/// `transport_cap`, the new path's own cap (the user's limit still applies).
	pub fn path_changed(&mut self, transport_cap: u32) {
		self.transport_cap = transport_cap.max(FLOOR_KBPS);
		let cap = if self.user_cap > 0 { self.user_cap.min(self.transport_cap) } else { self.transport_cap };
		self.cap = cap.max(FLOOR_KBPS);
		self.ceiling = self.cap;
		self.want = self.want.max(self.target).clamp(FLOOR_KBPS, self.cap);
		self.retarget();
		self.loss_cut = None;
		self.non_responsive = false;
		self.floor_windows = 0;
		self.floor_escapes = 0;
		self.bad_rate = 0;
		self.raise = None;
		self.probe_from = 0;
		self.clean = 0;
		self.at_ceiling = 0;
		self.probe_wait = PROBE_WAIT_MIN;
		self.probe_age = None;
		self.probe_step_pct = PROBE_STEP_MIN_PCT;
		self.soft_cut_at = None;
		self.soft_loss_at = None;
		self.soft_limit = None;
		self.over = 0;
		self.lossy = 0;
		self.noise_loss = 0.0;
		self.noise_ok_rate = 0;
		self.noise_dev = 0.0;
		self.probe_down = None;
		self.rtt_base = 0.0;
		self.rtt_ema = 0.0;
		self.rtt_last = 0.0;
		self.rtt_floor = 0.0;
		self.queued_prev = false;
		self.deadband = 0;
		self.deadband_acc = 0.0;
		self.startup_left = self.cfg.startup_windows;
		self.startup_paused = false;
		self.delay = Trendline::new();
		self.last_fast_cut_ms = -1.0e9;
		self.queue_cuts = 0;
		self.point_cap = 0;
		self.at_point_cap = 0;
		self.drain_left = 0;
		self.clean_at_target = 0;
		self.clean_at_point = 0;
		self.since_step = COOLDOWN_WINDOWS;
		self.encoder_kbps = self.encoder_rate();
	}

	/// The current bandwidth limit (`0` = none) and the effective cap.
	pub fn user_cap(&self) -> u32 {
		self.user_cap
	}
	pub fn cap(&self) -> u32 {
		self.cap
	}

	/// The user pinned (or unpinned) the bitrate: resync the target without punishment.
	pub fn set_target(&mut self, kbps: u32) {
		self.want = kbps.clamp(FLOOR_KBPS, self.cap);
		self.retarget();
		self.since_step = 0;
		self.encoder_kbps = self.encoder_rate();
	}

	/// The user pinned a resolution and/or an fps in the session menu (`None` = automatic).
	/// The ladder is narrowed to rungs that keep the pinned dimension(s) — a pinned 1080p
	/// still steps its fps down when the line congests, a pinned 120 fps still trades
	/// resolution — and the rate keeps adapting. A pinned size that isn't a rung (a display
	/// mode from the host's list) gets its own rungs, one per fps step.
	/// The user's pinned resolution and fps (`set_pins`), if any.
	pub fn pins(&self) -> (Option<(u32, u32)>, Option<u32>) {
		(self.pin_res, self.pin_fps)
	}

	pub fn set_pins(&mut self, res: Option<(u32, u32)>, fps: Option<u32>) {
		self.manual_point = false;
		self.pin_res = res.filter(|&(w, h)| w > 0 && h > 0);
		self.pin_fps = fps.filter(|&f| f > 0);
		self.rebuild_points();
	}

	/// The captured monitor changed (session menu monitor switch): rebuild the ladder for its
	/// native size / the fps target it can take, keeping the rate state.
	pub fn set_native(&mut self, w: u32, h: u32, fps: u32) {
		self.cfg.native = (w, h, fps);
		self.rebuild_points();
	}

	/// The codec changed (session menu): the rungs' minimum bitrates follow its efficiency.
	pub fn set_codec(&mut self, codec: VCodec) {
		self.cfg.codec = codec;
		self.rebuild_points();
	}

	fn rebuild_points(&mut self) {
		self.points = build_points(&self.cfg, self.pin_res, self.pin_fps);
		// Indices changed: a host-bound cap learned on the old ladder no longer maps.
		self.point_cap = 0;
		self.at_point_cap = 0;
		self.point_idx = if self.cfg.ladder { ladder::pick(&self.points, self.want, POINT_MARGIN) } else { 0 };
		self.point_age = 0;
		self.clean_at_point = 0;
		self.point_short = 0;
		self.last_point_window = self.windows;
		// The new rung's floor may differ (a lighter pick lets the rate go lower). The
		// encoder rate is left for the next tick to emit as a decision.
		self.retarget();
	}

	/// The user pinned a resolution/fps (`true`) or went back to automatic (`false`).
	pub fn set_manual_point(&mut self, manual: bool) {
		self.manual_point = manual;
		if !manual {
			self.point_idx = ladder::pick(&self.points, self.want, POINT_MARGIN).max(self.point_cap);
			self.point_age = 0;
		}
		self.retarget();
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
			|| !self.can_cut()
			|| self.drain_left > 0
			|| self.fast_cut_pending
		{
			return None;
		}
		self.last_fast_cut_ms = now_ms;
		// This is the window's cut: the tick that closes it doesn't cut again.
		self.fast_cut_pending = true;
		let before = self.target;
		let mut d = Decision { reason: "delay overuse (fast) → ×0.85", ..Default::default() };
		// Between ticks only the last window's RTT / loss can back the delay signal up.
		let confirmed = self.last.excess_ms >= RTT_OK_MS || self.last.eff_loss > LOSS_CLEAN;
		let _ = self.queue_cut(before, before * 85 / 100, confirmed, &mut d);
		self.apply_ladder(&mut d, true);
		self.sync_encoder(&mut d);
		Some(d)
	}

	// ── The window decision ─────────────────────────────────────────────────────────

	pub fn tick(&mut self, s: &Sample) -> Decision {
		self.windows += 1;
		if self.soft_limit.is_some_and(|(_, until)| self.windows > until) {
			self.soft_limit = None;
		}
		self.since_step = self.since_step.saturating_add(1);
		self.point_age = self.point_age.saturating_add(1);
		if let Some(a) = self.probe_age.as_mut() {
			*a += 1;
		}
		self.queue_cut_now = false;
		self.startup_climbed = false;
		let total = s.recv + s.lost;
		if total > 0 {
			self.data_windows += 1;
		}
		let loss = if total > 0 { s.lost as f32 / total as f32 } else { 0.0 };
		// The loss the rate logic judges — congestion triggers, the noise floor, the probe-down
		// and loss-cut verdicts: pre-repair on a measured window (`raw_loss`), the post-repair
		// loss otherwise. The recovery-mode flip and the clean-picture checks keep `loss`.
		self.measured = s.window_ms > 0;
		let raw = raw_loss(s, loss);
		// Goodput (video, kbit/s) and the host's estimated video send rate (what arrived, scaled
		// up by the loss). Only for a measured window: without `window_ms` every goodput rule
		// below stays off and the pre-2026-11-09 paths run (older callers, the mobile app).
		self.goodput = (s.window_ms > 0 && s.recv_bytes > 0 && s.recv > 0)
			.then(|| s.recv_bytes as f32 * 8.0 / s.window_ms as f32);
		self.send_est = self.goodput.map(|g| g * total as f32 / s.recv.max(1) as f32);
		let (rtt, excess, jitter) = self.update_rtt(&s.rtt_ms);
		// Mild bloat must persist for two windows before it cuts (a standing queue does; a
		// jitter burst that happened to cover one whole window doesn't). Severe bloat, or bloat
		// WITH loss (a queue already overflowing), cuts at once.
		let queued_before = self.queued_prev;
		self.queued_prev = excess >= RTT_OK_MS;
		if raw < self.noise_loss {
			self.noise_floor_down(raw);
		}
		let eff = (raw - self.noise_loss).max(0.0);
		let tol = self.loss_tolerance(total);
		// A large floor follows a slightly worse window up as it follows a better one down
		// (`noise_floor_down`), a quarter of the way: tracking only the lucky windows biased a
		// 35 % floor to ~32 %, so ordinary windows read as 3 % over it and drew probe-downs.
		// Only within the scatter (`tol`) — more than that is judged as loss over noise — and
		// only at a rate that already showed a window at or under the floor: a probe a few %
		// over a policer adds less than `tol` of overflow, which must not become "noise".
		if self.noise_loss >= LOSS_DOWN && total > 100 {
			// The floor's observed scatter (mean deviation, each window capped at `LOSS_DOWN` so a
			// congestion window can't inflate it): real radio loss is burstier than binomial.
			let dev = (raw - self.noise_loss).abs().min(LOSS_DOWN);
			self.noise_dev += (dev - self.noise_dev) * 0.1;
			if raw <= self.noise_loss {
				self.noise_ok_rate = self.target;
			} else if eff < tol && self.target <= self.noise_ok_rate {
				self.noise_loss += (raw - self.noise_loss) * 0.25;
			}
		}
		let delay = self.delay.state();
		self.last = Signals {
			loss,
			eff_loss: eff,
			raw_loss: raw,
			rtt_ms: rtt,
			excess_ms: excess,
			jitter_ms: jitter,
			delay,
			trend: self.delay.trend() as f32,
			goodput_kbps: self.goodput.unwrap_or(0.0),
			send_kbps: self.send_est.unwrap_or(0.0),
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
			pd.loss_acc += raw;
			if pd.windows < PROBE_DOWN_WINDOWS {
				self.probe_down = Some(pd);
				return self.finish(out);
			}
			let after = pd.loss_acc / pd.windows as f32;
			if !pd.measured && (pd.fec_overhead - self.fec_overhead).abs() > 1e-6 {
				// FEC changed under the probe: inconclusive — restore, judge again later.
				self.apply_rate(pd.from, &mut out);
				out.reason = "probe down inconclusive (fec changed) — restored";
			} else if pd.pre_loss - after >= (pd.pre_loss - pd.noise).max(0.0) * 0.4 && pd.pre_loss - after >= tol * 1.25 {
				// Followed = the loss over the floor fell by 40 % AND by more than the scatter of
				// one window against a two-window mean (`tol` × √1.5): the probe starts on a window
				// picked for being lossy, so on a 35 % floor the next ones read lower by chance.
				// Measured, that is a loss episode at `from` — congestion, whatever the parity did
				// meanwhile: the ceiling goes just under it and the rate stays down.
				if pd.measured {
					// The held rate only lifts the ceiling when it ran clean — by the clean
					// branch's measure for repaired loss (an unlearned 1 % random floor is not
					// "loss at this rate").
					let held = if after - pd.noise < LOSS_DOWN * 0.5 { pd.to } else { 0 };
					self.loss_episode(pd.from, held);
					if self.target > self.ceiling {
						let c = self.ceiling;
						self.apply_rate(c, &mut out);
					}
				} else {
					self.punish(pd.from);
				}
				out.reason = "sustained loss followed the rate — kept the lower rate";
			} else {
				// Never learn more than the probe's start showed: a lower rate still over the
				// capacity carries overflow loss that would otherwise ratchet the floor up.
				self.noise_loss = self.noise_loss.max(after.min(pd.pre_loss));
				self.noise_ok_rate = self.noise_ok_rate.max(pd.from);
				self.non_responsive = true;
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

		// Never stuck at the floor: steady loss (or none) there for `FLOOR_ESCAPE_WINDOWS` is
		// not something the rate causes — it can't go lower. Take that loss as the path's
		// baseline (noise), forget the ceiling the way down punished and probe up (the
		// 2026-11-09 session sat at 300 kbit/s for 6–8 minutes behind punished ceilings and
		// doubled probe waits). A loss that then follows the rate up is judged like a failed
		// probe (cut, ceiling punished again); escapes that keep failing wait twice as long.
		let floor = self.rate_min();
		let at_floor = total > 0 && self.target < self.cap && self.target <= floor + floor / 4;
		if at_floor && excess < RTT_EXCESS_MS && (self.floor_windows == 0 || raw <= self.floor_loss + 0.05) {
			if self.floor_windows == 0 {
				self.floor_loss = raw;
			}
			self.floor_windows += 1;
			let wait = FLOOR_ESCAPE_WINDOWS << self.floor_escapes.min(1);
			if self.floor_windows >= wait {
				self.floor_windows = 0;
				self.floor_escapes = self.floor_escapes.saturating_add(1);
				self.noise_loss = self.noise_loss.max(raw);
				self.noise_ok_rate = self.noise_ok_rate.max(self.target);
				self.ceiling = self.cap;
				self.bad_rate = 0;
				self.soft_limit = None;
				self.loss_cut = None;
				self.probe_age = Some(0);
				let before = self.target;
				self.probe_from = before;
				let next = (self.target as u64 * 3 / 2).min(self.cap as u64) as u32;
				self.apply_rate(next, &mut out);
				self.raise = Some((before, self.windows));
				out.reason = "stuck at the floor with steady loss → escape probe ×1.5";
				return self.finish(out);
			}
		} else {
			self.floor_windows = 0;
		}

		// One cut per window: the fast reflex already made this window's.
		if self.fast_cut_pending && (delay == DelayState::Overuse || excess >= RTT_EXCESS_MS || eff > LOSS_DOWN) {
			self.clean = 0;
			self.at_ceiling = 0;
			out.reason = "already cut this window (fast reflex) — hold";
			return self.finish(out);
		}

		// Delay first: the gradient says a queue is filling.
		let draining_host = self.drain_left > 0;
		if delay == DelayState::Overuse && self.can_cut() && self.since_step >= 1 && !draining_host {
			let before = self.target;
			// Backed up by the keepalive RTT or by loss = a real standing queue; a delay-only
			// overuse on a jittery radio link cuts the rate but doesn't lower the ceiling (once).
			let confirmed = excess >= RTT_OK_MS || eff > LOSS_CLEAN;
			self.queue_cut_now = true;
			let punished = self.queue_cut(before, before * 85 / 100, confirmed, &mut out);
			if out.reason.is_empty() {
				out.reason = if punished { "delay overuse → ×0.85" } else { "delay overuse (unconfirmed) → ×0.85, ceiling kept" };
			}
			return self.finish(out);
		}
		// Keepalive RTT over its baseline (and still over it at the end of the window).
		if excess >= RTT_EXCESS_MS
			&& (excess >= RTT_BAD_MS || queued_before || eff > LOSS_CLEAN)
			&& !draining
			&& !draining_host
			&& self.can_cut()
			&& self.since_step >= 1
		{
			let before = self.target;
			let cut = if excess >= RTT_BAD_MS { before / 2 } else { before * 7 / 10 };
			out.reason = if excess >= RTT_BAD_MS { "rtt bloat severe → halve" } else { "rtt bloat → ×0.7" };
			self.queue_cut_now = true;
			let _ = self.queue_cut(before, cut, true, &mut out);
			return self.finish(out);
		}
		// No queue-driven cut this window: the run (if any) is over.
		if !queued {
			self.queue_cuts = 0;
		}
		// Severe loss (over the learned noise floor — with no noise learned, the raw loss).
		if total > 100 && eff > LOSS_SEVERE && self.can_cut() {
			if let Some(send) = self.send_est {
				return self.severe_loss_measured(raw, send, queued || queued_before, out);
			}
			let before = self.target;
			// Halve at once either way (the picture needs the headroom NOW). Only a burst that
			// comes with a queue, lasts, or repeats within 30 s marks this rate as the link's
			// limit; a lone burst on a flat link (a radio blip, a Wi-Fi retry storm) does not.
			let repeat = self.soft_loss_at.is_some_and(|w| self.windows.saturating_sub(w) <= SOFT_REPEAT_WINDOWS);
			let confirmed =
				queued || queued_before || self.lossy >= 2 || repeat || self.in_probe() || !self.steady_rate();
			if confirmed {
				self.punish(before);
				self.soft_loss_at = None;
				out.reason = "severe loss → halve";
			} else {
				self.soft_loss_at = Some(self.windows);
				self.soft_reset();
				self.soft_limit = Some((before * 95 / 100, self.windows + SOFT_REPEAT_WINDOWS));
				out.reason = "severe loss burst (unqueued) → halve, ceiling kept";
			}
			self.apply_rate(before / 2, &mut out);
			return self.finish(out);
		}
		// Sustained mild loss.
		if total > 100 && eff > LOSS_DOWN {
			self.over += 1;
			self.clean = 0;
			self.at_ceiling = 0;
			self.clean_at_point = 0;
			// Loss over noise is a congestion sign: the startup ramp settles here.
			self.startup_left = 0;
			// Measured: loss right after a climb / probe step (or while a probe is judged) has an
			// obvious suspect — go straight back to the rate before the step and judge from
			// there, instead of riding the new rate for 2–3 lossy windows first (each one a
			// second or two of broken picture on the pol10 run).
			let raised = if self.measured {
				self.raise
					.filter(|&(from, at)| from < self.target && self.windows.saturating_sub(at) <= RAISE_SUSPECT_WINDOWS)
					.map(|(from, _)| from)
					.or_else(|| (self.in_probe() && self.probe_from < self.target).then_some(self.probe_from))
			} else {
				None
			};
			if (raised.is_some() || (self.over >= 2 && self.since_step >= COOLDOWN_WINDOWS)) && self.can_cut() {
				let before = self.target;
				if queued {
					self.punish(before);
					self.apply_rate(before * 7 / 10, &mut out);
					out.reason = "sustained loss with a queued link → ×0.7";
				} else {
					self.over = 0;
					let to = raised.unwrap_or(before * 4 / 5);
					self.probe_down = Some(ProbeDown {
						from: before,
						to,
						pre_loss: raw,
						windows: 0,
						loss_acc: 0.0,
						fec_overhead: self.fec_overhead,
						measured: self.measured,
						noise: self.noise_loss,
					});
					self.apply_rate(to, &mut out);
					out.reason = if raised.is_some() {
						"loss right after a rate step → back to the rate before it (judging)"
					} else {
						"sustained loss, flat link → probe down"
					};
				}
			}
			return self.finish(out);
		}
		// Clean window on an unqueued link: climb / probe. Measured, a window whose pre-repair
		// loss sits a little over the floor but that parity / NACK repaired completely counts
		// as clean too: random loss FEC covers (1 % on the pol10 run, never learned as a floor
		// because nothing cuts on it) must not stall the climb. More than `LOSS_DOWN / 2` over
		// the floor is not waved through — that is what a rate just over a policer looks like.
		let repaired_clean = self.measured && loss < LOSS_CLEAN && eff < LOSS_DOWN * 0.5;
		if total > 0 && (eff < tol || repaired_clean) && !queued && delay != DelayState::Underuse {
			self.over = 0;
			self.deadband = 0;
			self.deadband_acc = 0.0;
			// The path held the current rate cleanly: a cut the point floor held back is moot
			// (the point is sustained — no shortfall step for it).
			self.want = self.want.max(self.target);
			// A clean window after a loss cut: the cut worked (the loss followed the rate) —
			// the next loss is a new event, judged on its own.
			self.loss_cut = None;
			if self.probe_age.is_some_and(|a| a >= PROBE_PUNISH_WINDOWS) {
				// The probe survived: the next one may come sooner and go further.
				self.probe_age = None;
				self.floor_escapes = 0;
				self.probe_wait = (self.probe_wait / 2).max(PROBE_WAIT_MIN);
				self.probe_step_pct = (self.probe_step_pct * 2).min(PROBE_STEP_MAX_PCT);
				// Past the last loss episode's rate and clean: that limit is gone.
				if self.target >= self.bad_rate {
					self.bad_rate = 0;
				}
			}
			self.clean_at_point += 1;
			self.clean_at_target += 1;
			if self.clean_at_target >= LAST_GOOD_WINDOWS {
				self.last_good = self.target;
			}
			// A long clean stretch at the host-bound point: let the ladder try one heavier rung.
			if self.point_cap > 0 && self.point_idx == self.point_cap {
				self.at_point_cap += 1;
				if self.at_point_cap >= self.point_retry {
					self.at_point_cap = 0;
					self.point_cap -= 1;
				}
			}
			if self.target < self.ceiling {
				self.clean += 1;
				self.at_ceiling = 0;
				let far = (self.target as f32) < self.ceiling as f32 * FAST_CLIMB_SHARE;
				// After a loss episode the ceiling sits just under the rate that failed: walk up
				// to it in small steps, never one ×1.25 jump that lands on the old failure (the
				// pol10 run went 8.2 → 10.2 Mbit, the rate that had just failed, in one window).
				let gentle = self.bad_rate > 0;
				let (need, factor) = if self.startup_left > 0 {
					(1, STARTUP_FACTOR)
				} else if far {
					(2, 1.5)
				} else if gentle {
					(2, 1.0 + NEAR_BAD_STEP_PCT as f64 / 100.0)
				} else {
					(CLIMB_WINDOWS, 1.25)
				};
				let soft = self.soft_limit.map(|(lim, _)| lim.max(self.target));
				if self.clean >= need && soft.is_none_or(|lim| lim > self.target) {
					self.clean = 0;
					let before = self.target;
					let mut next = ((self.target as f64 * factor) as u32).min(self.ceiling);
					if let Some(lim) = soft {
						next = next.min(lim);
					}
					self.startup_climbed = self.startup_left > 0;
					self.apply_rate(next, &mut out);
					self.raise = Some((before, self.windows));
					out.reason = if self.startup_left > 0 {
						"startup probe ×1.5"
					} else if far {
						"clean, far under the ceiling → fast climb ×1.5"
					} else if gentle {
						"clean → gentle climb toward ceiling (+8 %, after a loss episode)"
					} else {
						"clean → climb toward ceiling"
					};
				}
			} else {
				self.clean = 0;
				self.at_ceiling += 1;
				if self.at_ceiling >= self.probe_wait && self.ceiling < self.cap {
					self.at_ceiling = 0;
					// A probe that would reach the last loss episode's rate goes there in a small
					// step: the growing steps are for a link that improved, not for re-testing a
					// limit seen minutes ago.
					let mut step = self.probe_step_pct;
					let reach = self.target as u64 * (100 + step) as u64 / 100;
					if self.bad_rate > 0 && reach >= self.bad_rate as u64 * 95 / 100 {
						step = step.min(NEAR_BAD_STEP_PCT);
					}
					let before = self.target;
					let next = (self.target as u64 * (100 + step) as u64 / 100).min(self.cap as u64) as u32;
					self.probe_from = self.ceiling;
					self.ceiling = next.max(self.ceiling);
					self.probe_age = Some(0);
					self.apply_rate(next, &mut out);
					self.raise = Some((before, self.windows));
					out.reason = "clean stretch at ceiling → probe past it";
				}
			}
			return self.finish(out);
		}
		// Deadband: a little loss, or the link slightly queued / draining → hold. Mild loss
		// that persists on a FLAT link is not congestion (the residual after FEC / NACK on a
		// noisy path): learn it as the noise floor so the climb can resume.
		self.over = 0;
		self.clean = 0;
		// Measured, a window parity / NACK fully repaired with its pre-repair loss within
		// `LOSS_DOWN` of the floor neither counts toward the probe wait nor restarts it: on a
		// 1 %-random path every tenth window scatters to 1.5–3 %, and the wait never ran out.
		if !(self.measured && loss < LOSS_CLEAN && eff <= LOSS_DOWN) {
			self.at_ceiling = 0;
		}
		// A queue is a congestion sign: the startup ramp settles here. Mild loss over noise
		// only pauses it once (this window doesn't climb): one lossy window early on (the
		// opening IDR burst, a Wi-Fi retry, the first NACK round) cost a clean LAN ~40 s to
		// the cap when it ended the ramp at 4 Mbit. A second one is the link talking.
		if queued {
			self.startup_left = 0;
		} else if total > 0 && eff >= tol && self.startup_left > 0 {
			if self.startup_paused {
				self.startup_left = 0;
			} else {
				self.startup_paused = true;
			}
		}
		if total > 100 && eff > 0.0 && eff <= LOSS_DOWN && !queued && delay != DelayState::Underuse {
			// Mild loss on a flat link still sustains the current point (see the clean branch).
			self.want = self.want.max(self.target);
			self.deadband += 1;
			self.deadband_acc += raw;
			if self.deadband >= DEADBAND_LEARN_WINDOWS {
				let mean = self.deadband_acc / self.deadband as f32;
				self.deadband = 0;
				self.deadband_acc = 0.0;
				if self.measured && self.can_cut() {
					// Measured: test it instead of assuming it. A policer's overflow is steady and
					// flat too — the old rule learned 2–3 % over a 10 Mbit policer as "noise" and
					// sat on it with 30 %+ pre-repair loss. A probe-down tells the two apart: loss
					// that follows the rate is a loss episode here, loss that stays is noise.
					let before = self.target;
					let to = before * 4 / 5;
					self.probe_down = Some(ProbeDown {
						from: before,
						to,
						pre_loss: mean,
						windows: 0,
						loss_acc: 0.0,
						fec_overhead: self.fec_overhead,
						measured: true,
						noise: self.noise_loss,
					});
					self.apply_rate(to, &mut out);
					out.reason = "steady mild loss on a flat link → probe down (noise or a limit?)";
				} else {
					self.noise_loss = self.noise_loss.max(raw);
					self.noise_ok_rate = self.noise_ok_rate.max(self.target);
					out.reason = "steady mild loss on a flat link — learned as noise";
				}
			}
		} else {
			self.deadband = 0;
			self.deadband_acc = 0.0;
		}
		self.finish(out)
	}

	/// Severe loss with a measured goodput (see the module doc, "Loss is measured").
	fn severe_loss_measured(&mut self, loss: f32, send: f32, queued: bool, mut out: Decision) -> Decision {
		let before = self.target;
		self.over = 0;
		self.clean = 0;
		self.at_ceiling = 0;
		self.clean_at_point = 0;
		self.startup_left = 0;
		let recent = self.loss_cut.filter(|lc| self.windows.saturating_sub(lc.window) <= LOSS_JUDGE_WINDOWS);
		if let Some(lc) = recent {
			// The cut's first window only half shows it (the re-request takes an RTT, the
			// window was already running): hold unless the loss got clearly worse.
			if self.windows.saturating_sub(lc.window) <= 1 && loss < lc.loss_before + 0.10 {
				out.reason = "severe loss — the last cut is still taking effect, hold";
				return self.finish(out);
			}
			// The pre-repair loss is clearly falling: the cut works, what is left is the
			// retransmits of the burst and the parity catching up crossing the bottleneck
			// (pol10: 31 → 24 → 17 → 11 → 2 % at a rate under the policer). Cutting again on it
			// halved a rate that was already right — and the share test below read the slow
			// fall as "did not follow".
			if loss <= lc.loss_before * 0.75 {
				out.reason = "severe loss falling after the last cut — hold";
				return self.finish(out);
			}
			// The host now sends clearly less (by `q`): did the delivered share follow? A
			// bottleneck keeps delivering its capacity, so the delivered share grows by ~1/q
			// (400 kbit link: 4 → 2 Mbit took the loss from 89 % to 80 % only, yet the share
			// delivered went ×1.8); random loss keeps the share (×1). Under the geometric middle
			// (√(1/q)) the loss is not congestion: learn it as noise, undo the run of cuts (rate
			// and ceiling) and stop cutting on it.
			let q = send / lc.send_before.max(1.0);
			let share_gain = (1.0 - loss).max(1e-3) / (1.0 - lc.loss_before).max(1e-3);
			if q <= NON_RESPONSIVE_RATE_DROP && share_gain < (1.0 / q.max(1e-3)).sqrt() {
				self.non_responsive = true;
				self.noise_loss = self.noise_loss.max(loss.min(lc.loss_before));
				self.noise_ok_rate = self.noise_ok_rate.max(lc.from);
				self.ceiling = self.ceiling.max(lc.from_ceiling).min(self.cap);
				self.bad_rate = lc.from_bad;
				self.soft_limit = None;
				self.loss_cut = None;
				self.apply_rate(lc.from, &mut out);
				out.reason = "loss did not follow the rate cut → non-congestive, learned as noise, restored";
				return self.finish(out);
			}
		}
		// The host sends far more than it was asked (keyframes, parity): a lower target does
		// not reach the wire — lighter frames do. Without a lighter rung, cut once and then hold.
		let enc_ref = self.win_enc_ref.max(self.encoder_kbps).max(1) as f32;
		if send > enc_ref * HOST_OVERSHOOT {
			if self.step_down_one(&mut out, "severe loss, host sends far over its rate → lighter point") {
				return self.finish(out);
			}
			if recent.is_some() {
				out.reason = "severe loss, host sends far over its rate (already cut) — hold";
				return self.finish(out);
			}
		}
		// GCC style: the new rate is a share of what the path DELIVERED (wire = video goodput
		// plus the parity share), not a blind halving — between a half and ×0.85 of the rate.
		let delivered = self.goodput.unwrap_or(0.0) * (1.0 + self.fec_overhead);
		// What the bottleneck carried before the learned random loss dropped its share: on a
		// 10 Mbit path with a 35 % floor only 6.5 Mbit arrives, and a ceiling there sat at
		// 70 % of the capacity for minutes. Max send rate = delivered / (1 − p), wherever on
		// the path the random loss happens.
		let delivered = delivered / (1.0 - self.noise_loss).max(0.3);
		let to = ((delivered * GOODPUT_BETA) as u32).clamp(before / 2, before * 85 / 100);
		let repeat = self.soft_loss_at.is_some_and(|w| self.windows.saturating_sub(w) <= SOFT_REPEAT_WINDOWS);
		let confirmed = queued || self.lossy >= 2 || repeat || self.in_probe() || !self.steady_rate();
		let (from, from_ceiling, from_bad) =
			recent.map_or((before, self.ceiling, self.bad_rate), |lc| (lc.from, lc.from_ceiling, lc.from_bad));
		if confirmed {
			self.punish(before);
			// The path just showed what it carries: the ceiling sits there, not at ×0.85 of a
			// rate it never delivered.
			self.ceiling = self.ceiling.min((delivered as u32).max(to)).max(FLOOR_KBPS);
			// A loss episode at `before`: the climb back approaches it in small steps (the
			// delivered estimate the ceiling sits on overshot the pol10 policer by 7 %).
			self.bad_rate = before;
			self.soft_loss_at = None;
			out.reason = "severe loss → 0.85 × delivered";
		} else {
			self.soft_loss_at = Some(self.windows);
			self.soft_reset();
			self.soft_limit = Some((before * 95 / 100, self.windows + SOFT_REPEAT_WINDOWS));
			out.reason = "severe loss burst (unqueued) → 0.85 × delivered, ceiling kept";
		}
		self.apply_rate(to, &mut out);
		self.loss_cut =
			Some(LossCut { window: self.windows, send_before: send, loss_before: loss, from, from_ceiling, from_bad });
		self.finish(out)
	}

	/// How far over the noise floor a window's loss may sit and still count as clean: the
	/// floor is a binomial average, so a window of `total` packets scatters around it by
	/// √(p·(1−p)/n), or by the scatter actually seen (`noise_dev`, ×1.25 ≈ σ) when the radio
	/// is burstier. Two of those (capped at `LOSS_DOWN`) — at 35 % on ~900 packets ±1.6 %
	/// alone kept most windows out of `LOSS_CLEAN` and the climb sat for a minute. No floor
	/// learned = `LOSS_CLEAN`, exactly as before.
	fn loss_tolerance(&self, total: u32) -> f32 {
		let p = self.noise_loss;
		if p <= 0.0 || total == 0 {
			return LOSS_CLEAN;
		}
		let sigma = (p * (1.0 - p) / total as f32).sqrt().max(self.noise_dev * 1.25);
		(2.0 * sigma).clamp(LOSS_CLEAN, LOSS_DOWN)
	}

	/// The learned noise floor met a window with less loss. A floor that clearly went away
	/// (under half of it) — or a small one — drops at once: real congestion must not hide under
	/// it. A large floor (30–40 % random radio loss, binomially noisy window to window) follows
	/// a lower window a quarter of the way only, or every slightly lucky window would turn the
	/// next average one into "loss over noise" and a cut.
	fn noise_floor_down(&mut self, loss: f32) {
		if loss < self.noise_loss * 0.5 || self.noise_loss < LOSS_DOWN {
			self.noise_loss = loss;
		} else {
			self.noise_loss -= (self.noise_loss - loss) * 0.25;
		}
		if self.noise_loss < LOSS_DOWN {
			self.non_responsive = false;
		}
	}

	/// One rung down the ladder (the next lighter point: fps first under a pinned size),
	/// debounced. `false` when the point is fixed, on the bottom rung, or just changed.
	fn step_down_one(&mut self, out: &mut Decision, reason: &'static str) -> bool {
		if !self.ladder_active() || self.point_idx + 1 >= self.points.len() || self.point_age < POINT_DEBOUNCE {
			return false;
		}
		self.point_idx += 1;
		self.point_age = 0;
		self.clean_at_point = 0;
		self.point_short = 0;
		self.retarget();
		self.sync_encoder(out);
		out.point = Some(self.points[self.point_idx]);
		out.reason = reason;
		true
	}

	// ── Internals ───────────────────────────────────────────────────────────────────

	fn finish(&mut self, mut out: Decision) -> Decision {
		// A startup window that climbed cleanly doesn't use the ramp up: it runs until the
		// first congestion sign (or `STARTUP_MAX_WINDOWS`), like a slow start.
		if !(self.startup_climbed && self.windows < STARTUP_MAX_WINDOWS) {
			self.startup_left = self.startup_left.saturating_sub(1);
		}
		self.drain_left = self.drain_left.saturating_sub(1);
		self.fast_cut_pending = false;
		let cur = self.points[self.point_idx];
		self.point_short = if self.ladder_active() && self.want < cur.min_kbps { self.point_short + 1 } else { 0 };
		self.apply_ladder(&mut out, false);
		// The encoder share may change with FEC even when the target holds — and a point step
		// moves the target to the new rung's floor after the rate was already decided.
		let enc = self.encoder_rate();
		if enc != self.encoder_kbps {
			self.encoder_kbps = enc;
			if out.bitrate.is_none() && out.reason.is_empty() {
				out.reason = "fec share changed";
			}
			out.bitrate = Some(enc);
		}
		// Startup-settle bookkeeping (also catches a fast-reflex cut made between ticks: the
		// target is simply lower than at the last window's end).
		if self.target < self.settle_prev_target {
			self.last_cut_window = self.windows;
		}
		if out.point.is_some() {
			self.last_point_window = self.windows;
		}
		if self.target != self.settle_prev_target || out.point.is_some() {
			self.last_change_window = self.windows;
		}
		self.settle_prev_target = self.target;
		self.win_enc_ref = self.encoder_kbps;
		out
	}

	/// The ladder moves (pins included) — false when the point is fixed.
	fn ladder_active(&self) -> bool {
		!self.manual_point && self.points.len() >= 2
	}

	/// The rate floor of the current point: its minimum while a lighter rung exists to step
	/// to, the fps-aware floor on the bottom rung / with a fixed point. Never above the cap.
	fn point_floor(&self) -> u32 {
		let p = self.points[self.point_idx];
		let abs = rate_floor_for_fps(p.fps);
		let f = if self.ladder_active() && self.point_idx + 1 < self.points.len() { p.min_kbps.max(abs) } else { abs };
		f.min(self.cap)
	}

	/// The lowest rate the session can go to at all: the bottom rung's fps-aware floor.
	fn rate_min(&self) -> u32 {
		let p = if self.ladder_active() { self.points[self.points.len() - 1] } else { self.points[self.point_idx] };
		rate_floor_for_fps(p.fps).min(self.cap)
	}

	/// The rate logic may still cut: the wanted rate is above the session's lowest rate.
	fn can_cut(&self) -> bool {
		self.want > self.rate_min()
	}

	/// `target` from `want` and the current point's floor.
	fn retarget(&mut self) {
		let t = self.want.max(self.point_floor()).min(self.cap).max(FLOOR_KBPS.min(self.cap));
		if t != self.target {
			self.clean_at_target = 0;
		}
		self.target = t;
	}

	/// Push a changed encoder rate into `out` (a point step can move the target).
	fn sync_encoder(&mut self, out: &mut Decision) {
		let enc = self.encoder_rate();
		if enc != self.encoder_kbps {
			self.encoder_kbps = enc;
			out.bitrate = Some(enc);
		}
	}

	fn encoder_rate(&self) -> u32 {
		((self.target as f32 * fec_policy::encoder_share_ratio(self.fec_overhead)) as u32)
			.max(FLOOR_KBPS.min(self.target))
	}

	fn update_rtt(&mut self, samples: &[f32]) -> (f32, f32, f32) {
		let mut jitter_acc = 0.0f32;
		let mut jitter_n = 0u32;
		let mut win_min = f32::INFINITY;
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
			win_min = win_min.min(x);
		}
		// The window's mean RTT (a cross-window EMA lagged a whole window after a queue
		// drained and turned the next mild-loss window into a false "congestion" cut).
		// With a single sample the previous value is blended in to damp one-off spikes.
		if n > 0 {
			let mean = sum / n as f32;
			self.rtt_ema = if self.rtt_ema <= 0.0 || n >= 2 { mean } else { (self.rtt_ema + mean) * 0.5 };
		}
		// Queue floor: the window's MINIMUM sample (≈ 4 per window at the 500 ms keepalive).
		// The excess is measured on the floor, not the mean: a standing queue lifts EVERY
		// sample, so it lifts the floor; path jitter (Wi-Fi power save, a mobile uplink, an
		// overlay VPN) only lifts some. Seen 2026-11-08: 22 ms base, 60–90 ms spikes, 0 % loss —
		// the mean read as "rtt bloat" every window, cut ×0.7 / halved even at 300 kbit/s and
		// parked a 50 Mbit link on the 360p floor. An empty window keeps the last floor.
		if win_min.is_finite() {
			self.rtt_floor = win_min;
		}
		let floor = if self.rtt_floor > 0.0 { self.rtt_floor } else { f32::INFINITY };
		// Slow upward re-baseline (≈ 2 % of the gap per window): a path whose floor really
		// moved stops looking "bloated" after a few minutes.
		if self.rtt_base > 0.0 && floor.is_finite() && floor > self.rtt_base {
			self.rtt_base += (floor - self.rtt_base) * 0.02;
		}
		let excess = if self.rtt_base > 0.0 && floor.is_finite() {
			(floor - self.rtt_base).max(0.0)
		} else {
			0.0
		};
		let jitter = if jitter_n > 0 { jitter_acc / jitter_n as f32 } else { 0.0 };
		(self.rtt_ema, excess, jitter)
	}

	/// A queue-driven cut (delay overuse / RTT bloat) from `before` to `to`. Only the first
	/// cut of a run lowers the ceiling; once the queue has survived `HOST_BOUND_CUTS` cuts in
	/// a row it is the host that can't keep up, so the ladder steps to a lighter point and
	/// the rate is left alone.
	///
	/// `confirmed` = the RTT / loss backs the queue up. An unconfirmed cut leaves the ceiling
	/// alone unless one already happened within `SOFT_REPEAT_WINDOWS` or a probe is being
	/// judged (then this rate IS the limit) — so a jittery link is not ratcheted down by
	/// one-off delay readings, but a real limit still stops the climb from sawtoothing.
	/// Returns whether the ceiling was punished (`false` = an unconfirmed, soft cut).
	fn queue_cut(&mut self, before: u32, to: u32, confirmed: bool, out: &mut Decision) -> bool {
		self.queue_cuts += 1;
		if self.queue_cuts >= HOST_BOUND_CUTS && self.step_down_host_bound(out) {
			self.queue_cuts = 0;
			return true;
		}
		let repeat = self.soft_cut_at.is_some_and(|w| self.windows.saturating_sub(w) <= SOFT_REPEAT_WINDOWS);
		let punished = confirmed || repeat || self.in_probe() || !self.steady_rate();
		if punished {
			let ceiling = self.ceiling;
			self.punish(before);
			if self.queue_cuts > 1 {
				self.ceiling = self.ceiling.max(ceiling);
			}
			self.soft_cut_at = None;
		} else {
			self.soft_cut_at = Some(self.windows);
			self.soft_reset();
			self.soft_limit = Some((before * 95 / 100, self.windows + SOFT_REPEAT_WINDOWS));
		}
		self.apply_rate(to, out);
		punished
	}

	/// The current rate had already run clean for a couple of windows (and the startup burst
	/// is over): a sudden queue / loss now is not something this rate change caused — the
	/// only case where an unconfirmed cut may spare the ceiling. Right after a climb or a
	/// probe, the new rate is the obvious suspect.
	fn steady_rate(&self) -> bool {
		self.clean_at_target >= 2 && self.startup_left == 0
	}

	/// A probe past the ceiling is still being judged.
	fn in_probe(&self) -> bool {
		self.probe_age.is_some_and(|a| a <= PROBE_PUNISH_WINDOWS)
	}

	/// An unconfirmed cut: no ceiling change, but restart the climb counters (and end the
	/// startup burst) like a punishment does.
	fn soft_reset(&mut self) {
		self.clean = 0;
		self.at_ceiling = 0;
		self.over = 0;
		self.clean_at_point = 0;
		self.startup_left = 0;
	}

	/// Move to the best rung that costs the host less (fewer pixels × fps, never more fps)
	/// and cap the ladder there. `false` when pinned or already on the lightest rung.
	fn step_down_host_bound(&mut self, out: &mut Decision) -> bool {
		if self.manual_point {
			return false;
		}
		let load = |p: &Point| p.width as u64 * p.height as u64 * p.fps as u64;
		let cur = self.points[self.point_idx];
		let Some(next) = (self.point_idx + 1..self.points.len())
			.find(|&i| load(&self.points[i]) < load(&cur) && self.points[i].fps <= cur.fps)
		else {
			return false;
		};
		self.point_retry = if self.point_retry == 0 {
			POINT_RETRY_WINDOWS
		} else {
			(self.point_retry * 2).min(POINT_RETRY_MAX)
		};
		self.point_idx = next;
		self.point_cap = next;
		self.at_point_cap = 0;
		self.point_age = 0;
		self.clean_at_point = 0;
		self.point_short = 0;
		self.retarget();
		self.since_step = 0;
		self.startup_left = 0;
		// The queue the heavier point built takes a moment to drain: don't read it as a new one.
		self.drain_left = 2;
		out.point = Some(self.points[next]);
		out.reason = "queue persists through rate cuts → host can't keep up, lighter point";
		true
	}

	/// The CLIENT's decoder can't keep up with the current point: `decode_ms` (its measured
	/// time per frame) uses more than 80 % of the frame budget. Steps to the best lighter rung
	/// with a LOWER fps whose estimated decode time (∝ pixels) fits 70 % of its budget, caps the
	/// ladder there (retried later like a host-bound cap) and returns that point change. An
	/// empty decision when the decoder keeps up, the point is pinned, or nothing lighter exists.
	/// (Seen 2026-11-08: a laptop's synchronous HEVC decoder held 1200p60 but not 90/120 — the
	/// backlog overflowed and the picture froze until the next keyframe.)
	pub fn client_decode_bound(&mut self, decode_ms: f32) -> Decision {
		let mut out = Decision::default();
		if self.manual_point || self.points.len() < 2 || !(decode_ms > 0.0) {
			return out;
		}
		let cur = self.points[self.point_idx];
		let budget = |fps: u32| 1000.0 / fps.max(1) as f32;
		if decode_ms < budget(cur.fps) * 0.8 {
			return out;
		}
		let px = |p: &Point| p.width as f32 * p.height as f32;
		let est = |p: &Point| decode_ms * px(p) / px(&cur).max(1.0);
		let lighter: Vec<usize> = (self.point_idx + 1..self.points.len())
			.filter(|&i| self.points[i].fps < cur.fps)
			.collect();
		let Some(&next) = lighter
			.iter()
			.find(|&&i| est(&self.points[i]) < budget(self.points[i].fps) * 0.7)
			.or(lighter.last())
		else {
			return out;
		};
		self.point_retry = if self.point_retry == 0 {
			POINT_RETRY_WINDOWS
		} else {
			(self.point_retry * 2).min(POINT_RETRY_MAX)
		};
		self.point_idx = next;
		self.point_cap = self.point_cap.max(next);
		self.at_point_cap = 0;
		self.point_age = 0;
		self.clean_at_point = 0;
		self.point_short = 0;
		self.retarget();
		self.last_point_window = self.windows;
		self.last_change_window = self.windows;
		out.point = Some(self.points[next]);
		out.reason = "client decoder can't keep up → lower fps";
		out
	}

	/// A rate produced loss/bloat: remember it as a punished ceiling and, if it was a probe,
	/// double the wait before the next one.
	fn punish(&mut self, bad_rate: u32) {
		self.ceiling = (bad_rate as u64 * 85 / 100).clamp(FLOOR_KBPS as u64, self.cap as u64) as u32;
		if self.probe_age.is_some_and(|a| a <= PROBE_PUNISH_WINDOWS) {
			self.probe_wait = (self.probe_wait * 2).min(PROBE_WAIT_MAX);
		}
		self.probe_step_pct = PROBE_STEP_MIN_PCT;
		self.probe_age = None;
		self.soft_limit = None;
		self.clean = 0;
		self.at_ceiling = 0;
		self.over = 0;
		self.clean_at_point = 0;
		self.startup_left = 0;
	}

	/// A loss episode at `rate` proven congestive on a measured path: a cut to `held` cured
	/// the pre-repair loss. The ceiling goes to `EPISODE_CEILING_PCT` of `rate` — or to `held`
	/// when that is higher (the rate before a small climb / probe step: it ran clean before the
	/// step, and a probe's start ran clean for the whole probe wait) — and `rate` becomes the
	/// known-bad level the next climb / probe approaches in small steps. A failed probe doubles
	/// the wait before the next one (`punish`), so a limit that stays where it is gets re-tested
	/// at 1, 2, 4, … minutes (up to `PROBE_WAIT_MAX`), never every few seconds.
	fn loss_episode(&mut self, rate: u32, held: u32) {
		self.punish(rate);
		let at = (rate as u64 * EPISODE_CEILING_PCT as u64 / 100) as u32;
		self.ceiling = at.max(held.min(rate)).clamp(FLOOR_KBPS, self.cap);
		self.bad_rate = rate;
	}

	/// The rate logic decided `kbps`: that is the wanted rate (never under the session's
	/// lowest rate, never over the cap); the target follows it down to the current point's
	/// floor (`retarget`) — under that the ladder steps the point down instead.
	fn apply_rate(&mut self, kbps: u32, out: &mut Decision) {
		self.since_step = 0;
		// Any rate change ends the last step's suspect window (a climb / probe re-arms it).
		self.raise = None;
		self.want = kbps.max(self.rate_min()).min(self.cap);
		self.retarget();
		self.sync_encoder(out);
	}

	/// Move along the ladder for the current target. `urgent` (a fast cut) ignores the
	/// debounce when the shortfall is severe.
	fn apply_ladder(&mut self, out: &mut Decision, urgent: bool) {
		if !self.ladder_active() {
			return;
		}
		let cur = self.points[self.point_idx];
		let target = self.target;
		let want = self.want;
		// Down: the wanted rate no longer buys a clean picture at this point (the target
		// itself is held at the point's minimum meanwhile — see `point_floor`).
		if want < cur.min_kbps && self.point_idx + 1 < self.points.len() {
			let debounced = self.point_age >= POINT_DEBOUNCE;
			let go = match self.goodput {
				// Unmeasured window: the pre-2026-11-09 rule.
				None => debounced || urgent || (want as f32) < cur.min_kbps as f32 * 0.7,
				// Measured: a queue can't wait (debounced as before); a loss shortfall steps
				// after `POINT_SHORT_WINDOWS` in a row, or at once when the path delivered
				// clearly under the point's minimum — one bad window on a radio link doesn't
				// cost the user's resolution.
				Some(g) => {
					let delivered = g * (1.0 + self.fec_overhead);
					urgent
						|| delivered < cur.min_kbps as f32 * POINT_MARGIN
						|| (debounced && (self.queue_cut_now || self.point_short >= POINT_SHORT_WINDOWS))
				}
			};
			if go {
				let to = ladder::pick(&self.points, want, POINT_MARGIN).max(self.point_idx + 1);
				self.point_idx = to.min(self.points.len() - 1);
				self.point_age = 0;
				self.clean_at_point = 0;
				self.point_short = 0;
				self.retarget();
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
			let want = ladder::pick(&self.points, target, 1.0 / UP_HEADROOM).max(self.point_cap);
			// Not on a probe's rate: a probe that fails a window later would make the step
			// up an immediate step back down (two encoder restarts for nothing).
			if want < self.point_idx && self.clean_at_point >= need && self.point_age >= POINT_DEBOUNCE && !self.in_probe() {
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

	/// Test config with the ladder ON (these tests exercise it; the app default is off).
	/// Starts at the cap (a remembered rate), as these tests did before the cautious
	/// no-memory start (`START_KBPS_NO_MEMORY`); the start itself has its own tests.
	fn cfg(cap: u32, ir: bool) -> Config {
		let mut c = Config::new(VCodec::H264, cap);
		c.ir_capable = ir;
		c.ladder = true;
		c.start_kbps = cap;
		c
	}

	/// `Config::new` starting at the cap (see `cfg`).
	fn at_cap(codec: VCodec, cap: u32) -> Config {
		let mut c = Config::new(codec, cap);
		c.start_kbps = cap;
		c
	}

	/// Game mode (`min_fps` 60): a thin path keeps 60 fps by giving up resolution — the
	/// 3 Mbit mobile case that used to land on 720p30.
	#[test]
	fn game_mode_trades_resolution_not_fps() {
		let mut c = cfg(15_000, false);
		c.codec = VCodec::H265;
		c.native = (2560, 1440, 120);
		let free = build_points(&c, None, None);
		let at = |pts: &[Point], kbps: u32| pts[ladder::pick(pts, kbps, POINT_MARGIN)];
		assert_eq!(at(&free, 3_000).fps, 30, "without the floor a thin path drops to 30 fps");
		c.min_fps = 60;
		let game = build_points(&c, None, None);
		for kbps in [1_200, 2_000, 3_000, 5_000, 9_000] {
			let p = at(&game, kbps);
			assert!(p.fps >= 60, "{kbps} kbps picked {}", p.label());
		}
		assert_eq!(at(&game, 3_000).height, 540, "3 Mbit buys 540p60 in game mode");
		// Only the cheapest rung stays below the floor, as a last resort.
		let low: Vec<String> = game.iter().filter(|p| p.fps < 60).map(|p| p.label()).collect();
		assert_eq!(low, vec!["360p30".to_string()]);
		// A user-pinned fps is left alone.
		let pinned = build_points(&c, None, Some(30));
		assert!(pinned.iter().all(|p| p.fps == 30));
		// With a 720p floor too (the desktop's game mode): never under 720p and never under
		// 60 fps; a budget below 720p60's minimum stays on 720p60 at a lower bitrate.
		c.min_height = 720;
		let floor = build_points(&c, None, None);
		let labels: Vec<String> = floor.iter().map(|p| p.label()).collect();
		assert!(floor.iter().all(|p| p.height >= 720 && p.fps >= 60), "{labels:?}");
		assert_eq!(at(&floor, 3_000).label(), "720p60");
		assert_eq!(at(&floor, 1_500).label(), "720p60");
	}

	fn win(recv: u32, lost: u32, rtt: f32) -> Sample {
		Sample { recv, lost, rtt_ms: vec![rtt; 4], ..Default::default() }
	}

	/// A jittery path with NO queue and NO loss (Wi-Fi / mobile uplink / overlay VPN):
	/// 22 ms floor, but most windows carry 60–90 ms spikes. The window mean sits far over the
	/// baseline; the floor doesn't. The rate must hold (or climb), never ride down to the floor.
	#[test]
	fn jitter_spikes_without_loss_do_not_collapse_the_rate() {
		let mut c = Controller::new(cfg(15_000, false));
		c.set_target(3_000);
		let start = c.target_kbps();
		for w in 0..60u32 {
			let spikes: Vec<f32> = match w % 3 {
				0 => vec![22.0, 85.0, 90.0, 23.0],
				1 => vec![60.0, 22.0, 75.0, 88.0],
				_ => vec![22.0, 24.0, 21.0, 23.0],
			};
			c.tick(&Sample { recv: 400, lost: 0, rtt_ms: spikes, ..Default::default() });
		}
		assert!(c.target_kbps() >= start, "jitter cut the rate: {} < {start}", c.target_kbps());
	}

	/// Clean path: the opening settles within a few windows; a lossy start only after two
	/// cut-free windows.
	#[test]
	fn startup_settles_once_the_rate_stops_moving() {
		let mut c = Controller::new(cfg(15_000, false));
		assert!(!c.startup_settled(), "never settled before any window");
		let mut at = None;
		for w in 0..8u32 {
			c.tick(&win(1500, 0, 20.0));
			if at.is_none() && c.startup_settled() {
				at = Some(w);
			}
		}
		assert!(matches!(at, Some(w) if w <= 3), "clean path settled at {at:?}");

		// A low start climbs first: not settled while the ramp still raises the rate.
		let mut low = cfg(15_000, false);
		low.start_kbps = 2_000;
		let mut c = Controller::new(low);
		let mut prev = c.target_kbps();
		for _ in 0..12 {
			c.tick(&win(1500, 0, 20.0));
			if c.target_kbps() != prev {
				assert!(!c.startup_settled(), "settled mid-climb at {}", c.target_kbps());
			}
			prev = c.target_kbps();
		}
		assert!(c.startup_settled(), "settled once the climb stopped at {}", c.target_kbps());

		let mut c = Controller::new(cfg(15_000, false));
		c.tick(&win(1000, 0, 20.0));
		c.tick(&win(600, 400, 20.0)); // 40 % loss → cut
		c.tick(&win(600, 400, 20.0)); // and again
		assert!(!c.startup_settled(), "a window that cut is not settled");
		c.tick(&win(1000, 0, 20.0));
		assert!(!c.startup_settled(), "one clean window after a cut is not enough");
		c.tick(&win(1000, 0, 20.0));
		c.tick(&win(1000, 0, 20.0));
		assert!(c.startup_settled(), "two cut-free windows later it is");
	}

	/// A decoder that needs 12 ms per 1200p frame can't hold 120 fps (8.3 ms budget) or 90
	/// (11.1 ms): step down to a rung it can, and leave a fast decoder alone.
	#[test]
	fn slow_client_decoder_steps_the_fps_down() {
		let mut c = cfg(15_000, false);
		c.codec = VCodec::H265;
		c.native = (1920, 1200, 120);
		c.min_height = 1080;
		c.min_kbps_scale = 0.5;
		let mut ctl = Controller::new(c);
		assert_eq!(ctl.point().fps, 120);
		assert!(ctl.client_decode_bound(4.0).point.is_none(), "4 ms keeps up at 120 fps");
		let d = ctl.client_decode_bound(12.0);
		let p = d.point.expect("stepped down");
		assert!(p.fps <= 60, "{p:?}");
		assert!(12.0 * (p.width * p.height) as f32 / (1920.0 * 1200.0) < 1000.0 / p.fps as f32 * 0.7, "{p:?}");
		// Capped: a clean climb doesn't immediately put it back on a heavier rung.
		for _ in 0..20 {
			ctl.tick(&win(2000, 0, 20.0));
		}
		assert!(ctl.point().fps <= p.fps, "{:?}", ctl.point());
	}

	/// A real standing queue lifts EVERY sample — the floor-based check must still cut.
	#[test]
	fn standing_queue_still_cuts_on_rtt_bloat() {
		let mut c = Controller::new(cfg(15_000, false));
		c.set_target(8_000);
		for _ in 0..4 {
			c.tick(&win(1000, 0, 22.0));
		}
		let before = c.target_kbps();
		for _ in 0..6 {
			c.tick(&win(1000, 0, 22.0 + 70.0));
		}
		assert!(c.target_kbps() < before, "queue not cut: {} >= {before}", c.target_kbps());
	}

	/// Remote-desktop ladder: no rung under 1080p (only fps/bitrate adapt), and the scaled
	/// minimums let a ~2 Mbit start already sit on a 1080p rung.
	#[test]
	fn remote_ladder_keeps_fhd_and_scales_minimums() {
		let mut c = cfg(15_000, false);
		c.codec = VCodec::H265;
		c.native = (1920, 1080, 60);
		c.min_height = 1080;
		c.min_kbps_scale = 0.5;
		c.start_kbps = 2_000;
		let ctl = Controller::new(c);
		assert!(ctl.points().iter().all(|p| p.height >= 1080), "{:?}", ctl.points());
		assert_eq!(ctl.point().height, 1080);
		// A native below the floor keeps its own top rung instead of an empty ladder.
		let mut small = cfg(15_000, false);
		small.native = (1280, 720, 60);
		small.min_height = 1080;
		let ctl = Controller::new(small);
		assert!(!ctl.points().is_empty());
		assert!(ctl.points().iter().all(|p| p.height == 720));
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
		c.ladder = true;
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
		c.set_fec_overhead(0.25);
		let d = c.tick(&win(1000, 0, 20.0));
		assert_eq!(d.bitrate, Some(6400), "8000 / 1.25: {d:?}");
	}

	#[test]
	fn ladder_is_off_by_default_and_the_point_is_the_native_size() {
		let c = Controller::new(at_cap(VCodec::H264, 8000));
		assert_eq!(c.point().label(), "1080p60");
		let mut c = c;
		let d = c.tick(&win(800, 200, 20.0));
		assert!(d.bitrate.is_some() && d.point.is_none(), "{d:?}");
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

	/// Never under the session's lowest rate — the bottom rung's fps-aware floor (360p30:
	/// 360 kbit/s, not 300) — and never above the cap.
	#[test]
	fn never_below_floor_and_never_above_cap() {
		let mut c = Controller::new(cfg(3000, false));
		let floor = rate_floor_for_fps(30);
		for _ in 0..8 {
			c.tick(&win(500, 500, 300.0));
			assert!(c.target_kbps() >= floor, "{}", c.target_kbps());
		}
		assert_eq!(c.target_kbps(), floor);
		assert_eq!(c.point().label(), "360p30");
		for _ in 0..200 {
			c.tick(&win(500, 500, 300.0));
			assert!(c.target_kbps() >= floor && c.target_kbps() <= 3000, "{}", c.target_kbps());
		}
		for _ in 0..2000 {
			c.tick(&win(1000, 0, 20.0));
		}
		assert!(c.target_kbps() <= 3000);
	}

	/// A host whose encoder sustains at most `max_load` pixels×fps per second, on a clean
	/// path: over it every frame leaves later than the last (a growing host-side queue that
	/// shows as delay overuse and RTT bloat at ANY bitrate); at or under it, flat. Runs one
	/// 2 s window (with the fast reflex polled every 200 ms) and returns its decisions.
	struct HostSim {
		t_ms: f64,
		frame: u64,
		lag_ms: f64,
	}
	impl HostSim {
		fn new() -> Self {
			Self { t_ms: 0.0, frame: 0, lag_ms: 0.0 }
		}
		fn window(&mut self, c: &mut Controller, max_load: u64) -> Vec<Decision> {
			let p = c.point();
			let load = p.width as u64 * p.height as u64 * p.fps as u64;
			let fps = p.fps.max(1) as f64;
			let mut out = Vec::new();
			let end = self.t_ms + 2000.0;
			let mut next_poll = self.t_ms + 200.0;
			while self.t_ms < end {
				self.t_ms += 1000.0 / fps;
				self.frame += 1;
				if load > max_load {
					self.lag_ms += 2.0;
				} else {
					self.lag_ms = (self.lag_ms - 8.0).max(0.0);
				}
				c.on_frame((self.t_ms * 90.0) as u32, self.t_ms + 20.0 + self.lag_ms);
				if self.t_ms >= next_poll {
					next_poll += 200.0;
					if let Some(d) = c.poll_fast(self.t_ms) {
						out.push(d);
					}
				}
			}
			let pkts = (c.encoder_kbps() / 8).max(50);
			out.push(c.tick(&win(pkts, 0, 20.0 + self.lag_ms as f32)));
			out
		}
	}

	fn load(p: Point) -> u64 {
		p.width as u64 * p.height as u64 * p.fps as u64
	}

	#[test]
	fn host_that_cannot_encode_1080p120_steps_fps_down_instead_of_starving_the_rate() {
		// The 2026-10-29 session: a software-encoding host asked for 1080p120 over a clean
		// P2P path. The old controller read the host's queue as congestion and cut the rate
		// to ~400 kbit at 1080p120 (1 fps on screen) and never climbed back.
		let mut cfg = cfg(15_000, true);
		cfg.native = (1920, 1080, 120);
		let mut c = Controller::new(cfg);
		assert_eq!(c.point().label(), "1080p120");
		let limit = 1920 * 1080 * 60;
		let mut sim = HostSim::new();
		let mut relieved_at = None;
		let mut over: Vec<u32> = Vec::new();
		for w in 0..400u32 {
			sim.window(&mut c, limit);
			if load(c.point()) <= limit {
				relieved_at.get_or_insert(w);
			} else if relieved_at.is_some() {
				over.push(w);
			}
		}
		assert!(matches!(relieved_at, Some(w) if w <= 5), "relieved at {relieved_at:?}");
		// A heavier point may be retried after a long clean stretch: each retry is a short
		// excursion, and once one failed the next waits at least `POINT_RETRY_WINDOWS`.
		let mut runs: Vec<(u32, u32)> = Vec::new();
		for &w in &over {
			match runs.last_mut() {
				Some((_, end)) if w == *end + 1 => *end = w,
				_ => runs.push((w, w)),
			}
		}
		assert!(runs.iter().all(|(a, b)| b - a <= 2), "short excursions: {runs:?}");
		assert!(runs.first().is_none_or(|(a, _)| *a >= 30), "no retry before a long clean stretch: {runs:?}");
		for pair in runs.windows(2) {
			assert!(pair[1].0 - pair[0].1 >= POINT_RETRY_WINDOWS, "retry wait: {runs:?}");
		}
		assert!(runs.len() <= 2, "the wait doubles: {runs:?}");
		assert!(load(c.point()) <= limit && c.point().fps <= 120, "{}", c.point().label());
		assert!(
			c.target_kbps() >= c.point().min_kbps,
			"the rate buys the point it settled on: {} @ {}",
			c.target_kbps(),
			c.point().label()
		);
	}

	#[test]
	fn a_burst_of_queue_cuts_lowers_the_ceiling_once_and_the_rate_climbs_back_fast() {
		let mut c = Controller::new(at_cap(VCodec::H264, 15_000));
		for _ in 0..5 {
			c.tick(&win(1500, 0, 20.0));
		}
		// A transient queue: four bloated windows in a row.
		for _ in 0..4 {
			c.tick(&win(1500, 0, 160.0));
		}
		assert!(c.target_kbps() < 2000, "{}", c.target_kbps());
		assert!(c.ceiling() >= 12_000, "only the first cut of the run punishes: {}", c.ceiling());
		let mut back_at = None;
		for w in 0..40 {
			c.tick(&win(1500, 0, 20.0));
			if back_at.is_none() && c.target_kbps() >= c.ceiling() / 2 {
				back_at = Some(w);
			}
		}
		// Fast to half the ceiling (startup pace), then the calm climb to the ceiling itself.
		assert!(matches!(back_at, Some(w) if w <= 10), "{back_at:?}");
		assert_eq!(c.target_kbps(), c.ceiling(), "back at the ceiling within 80 s");
	}

	/// A lone loss burst on an unqueued link (radio blip) halves the rate but is not the
	/// link's limit: the ceiling stays and the rate is back near it within ~half a minute.
	#[test]
	fn a_lone_loss_burst_halves_but_keeps_the_ceiling() {
		let mut c = Controller::new(at_cap(VCodec::H265, 15_000));
		for _ in 0..8 {
			c.tick(&win(2000, 0, 30.0));
		}
		assert_eq!(c.target_kbps(), 15_000);
		let d = c.tick(&win(1400, 600, 30.0)); // 30 % loss for one window, RTT flat
		assert_eq!(d.bitrate, Some(7_500), "{d:?}");
		assert_eq!(c.ceiling(), 15_000, "ceiling kept");
		let mut back = None;
		for w in 0..30 {
			c.tick(&win(2000, 0, 30.0));
			if back.is_none() && c.target_kbps() >= 14_000 {
				back = Some(w);
			}
		}
		assert!(matches!(back, Some(w) if w <= 15), "back near 15 Mbit within 30 s: {back:?} {}", c.target_kbps());
	}

	/// The same burst twice in 30 s IS the limit: the ceiling drops.
	#[test]
	fn a_repeated_loss_burst_lowers_the_ceiling() {
		let mut c = Controller::new(at_cap(VCodec::H265, 15_000));
		for _ in 0..8 {
			c.tick(&win(2000, 0, 30.0));
		}
		c.tick(&win(1400, 600, 30.0));
		for _ in 0..4 {
			c.tick(&win(2000, 0, 30.0));
		}
		let before = c.target_kbps();
		c.tick(&win(1400, 600, 30.0));
		assert!(c.ceiling() < 15_000, "ceiling {}", c.ceiling());
		assert_eq!(c.ceiling(), before * 85 / 100);
	}

	/// After a REAL limit (confirmed congestion) the link frees up: growing probes take the
	/// rate back to the cap in under 5 minutes, not 13+.
	#[test]
	fn a_freed_link_is_climbed_back_with_growing_probes() {
		let mut c = Controller::new(at_cap(VCodec::H265, 15_000));
		// A 5 Mbit bottleneck long enough to learn it (queue + loss above 5 Mbit).
		for _ in 0..60 {
			let s = capped(c.encoder_kbps(), 5_000, 30.0);
			c.tick(&s);
		}
		assert!(c.ceiling() <= 6_000, "learned the bottleneck: {}", c.ceiling());
		// The bottleneck goes away.
		let mut at_cap = None;
		for w in 0..200 {
			c.tick(&win(2500, 0, 30.0));
			if at_cap.is_none() && c.target_kbps() >= 15_000 {
				at_cap = Some(w);
			}
		}
		// +10 % → +20 % → +40 % → +80 % at one probe per minute: ~4.6 min (was ≥ 13 min at
		// +10 % per probe).
		assert!(matches!(at_cap, Some(w) if w <= 150), "at the cap within 5 min: {at_cap:?} {}", c.target_kbps());
	}

	/// A delay-only overuse (RTT flat, no loss) cuts the rate but keeps the ceiling; the same
	/// thing again within 30 s does lower it.
	#[test]
	fn a_lone_delay_overuse_keeps_the_ceiling_a_repeat_lowers_it() {
		let mut c = Controller::new(at_cap(VCodec::H265, 12_000));
		let mut t = 0.0f64;
		let mut feed = |c: &mut Controller, frames: u32, slope_ms: f64| {
			for i in 0..frames {
				t += 16.667;
				c.on_frame((t * 90.0) as u32, t + 20.0 + i as f64 * slope_ms);
			}
		};
		for _ in 0..5 {
			feed(&mut c, 120, 0.0);
			c.tick(&win(1500, 0, 20.0));
		}
		assert_eq!(c.ceiling(), 12_000);
		feed(&mut c, 120, 1.0);
		let d = c.tick(&win(1500, 0, 20.0));
		assert!(d.bitrate.is_some(), "cut on overuse: {d:?}");
		assert_eq!(c.ceiling(), 12_000, "unconfirmed → ceiling kept");
		c.on_stream_restart();
		for _ in 0..3 {
			feed(&mut c, 120, 0.0);
			c.tick(&win(1500, 0, 20.0));
		}
		let before = c.target_kbps();
		feed(&mut c, 120, 1.0);
		c.tick(&win(1500, 0, 20.0));
		assert!(c.ceiling() < 12_000 && c.ceiling() <= before, "repeat within 30 s → punished: {}", c.ceiling());
	}

	/// The user's bandwidth limit is a ceiling the controller adapts under, not a fixed rate.
	#[test]
	fn a_user_limit_caps_the_rate_but_adaptation_continues_under_it() {
		let mut c = Controller::new(cfg(50_000, true));
		for _ in 0..10 {
			c.tick(&win(3000, 0, 20.0));
		}
		assert_eq!(c.target_kbps(), 50_000);
		c.set_user_cap(10_000);
		assert_eq!(c.cap(), 10_000);
		assert_eq!(c.target_kbps(), 10_000, "clamped at once");
		// Congestion under the limit still cuts.
		let d = c.tick(&win(1500, 0, 160.0));
		assert!(d.bitrate.is_some_and(|b| b < 10_000), "{d:?}");
		for _ in 0..60 {
			c.tick(&win(1500, 0, 20.0));
		}
		assert!(c.target_kbps() <= 10_000 && c.target_kbps() >= 9_000, "{}", c.target_kbps());
		// Back to automatic: the old limit no longer holds it.
		c.set_user_cap(0);
		assert_eq!(c.cap(), 50_000);
		for _ in 0..120 {
			c.tick(&win(3000, 0, 20.0));
		}
		assert!(c.target_kbps() > 10_000, "{}", c.target_kbps());
	}

	/// A relayed session that collapsed on a congested relay and then upgraded to a direct
	/// path climbs to the new path's cap within a startup-like stretch, not one probe a minute.
	#[test]
	fn a_path_upgrade_forgets_the_old_path() {
		let mut c = Config::new(VCodec::H265, 8_000);
		c.ladder = true;
		c.native = (1920, 1200, 120);
		let mut c = Controller::new(c);
		// The relay queues badly: repeated confirmed overuse punishes the ceiling to the floor.
		for _ in 0..40 {
			c.tick(&win(800, 0, 140.0));
			c.tick(&win(800, 0, 40.0));
		}
		assert!(c.target_kbps() < 2_000, "collapsed: {}", c.target_kbps());
		// Direct path, 28 Mbit cap, clean 5 ms RTT.
		c.path_changed(28_000);
		assert_eq!(c.cap(), 28_000);
		let mut n = 0;
		while c.target_kbps() < 25_000 && n < 40 {
			c.tick(&win(3000, 0, 5.0));
			n += 1;
		}
		assert!(c.target_kbps() >= 25_000, "after {n} windows: {}", c.target_kbps());
		assert!(n <= 25, "took {n} windows (~{} s)", n * 2);
		assert!(c.point().fps >= 60 && c.point().height >= 1080, "{}", c.point().label());
	}

	/// The user's limit survives a path change.
	#[test]
	fn a_path_upgrade_keeps_the_user_limit() {
		let mut c = Controller::new(cfg(8_000, true));
		c.set_user_cap(5_000);
		c.path_changed(40_000);
		assert_eq!(c.cap(), 5_000);
		c.set_user_cap(0);
		assert_eq!(c.cap(), 40_000);
	}

	/// Pins are the preferred maximum (2026-11-09; they used to narrow the ladder to the
	/// pinned dimension): the pick is the top rung, a pinned size steps its fps down first and
	/// then the size, a pinned fps steps the size down first and then the fps.
	#[test]
	fn pins_cap_the_ladder_and_give_way_in_the_unpinned_dimension_first() {
		let mut c = at_cap(VCodec::H265, 20_000);
		c.ladder = true;
		c.native = (1920, 1200, 120);
		let mut c = Controller::new(c);
		let size = |p: &Point| (p.width, p.height);
		c.set_pins(Some((1920, 1200)), None);
		let pts = c.points().to_vec();
		assert_eq!((size(&pts[0]), pts[0].fps), ((1920, 1200), 120), "{pts:?}");
		let at_size: Vec<u32> = pts.iter().take_while(|p| size(p) == (1920, 1200)).map(|p| p.fps).collect();
		assert_eq!(at_size, vec![120, 90, 60, 30], "fps first: {pts:?}");
		assert!(pts[at_size.len()..].iter().all(|p| p.height < 1200 && p.fps == 30), "then the size: {pts:?}");
		assert!(pts.len() > at_size.len());
		assert!(pts.windows(2).all(|w| w[0].min_kbps > w[1].min_kbps), "a chain pick() can walk: {pts:?}");
		// A thin budget picks a lighter rung under the pin (the pin is no longer the only rung).
		c.set_target(3_000);
		c.set_pins(Some((1920, 1200)), None);
		assert!(c.point().min_kbps as f32 <= 3_000.0 * POINT_MARGIN, "{}", c.point().label());
		// Only the fps pinned: the size gives way first.
		c.set_pins(None, Some(120));
		let pts = c.points().to_vec();
		assert_eq!((size(&pts[0]), pts[0].fps), ((1920, 1200), 120));
		let at_fps: Vec<u32> = pts.iter().take_while(|p| p.fps == 120).map(|p| p.height).collect();
		assert!(at_fps.len() >= 3 && at_fps.windows(2).all(|w| w[0] > w[1]), "{pts:?}");
		let low = *at_fps.last().unwrap();
		assert!(pts[at_fps.len()..].iter().all(|p| p.height == low && p.fps < 120), "then the fps: {pts:?}");
		// A display mode that isn't a rung gets its own rungs on top.
		c.set_pins(Some((2560, 1440)), None);
		assert_eq!(size(&c.points()[0]), (2560, 1440));
		assert!(c.points().iter().all(|p| p.height <= 1440));
		c.set_pins(None, None);
		assert!(c.points().len() > 6, "back to the full ladder");
	}

	/// A monitor switch rebuilds the ladder for the new native size; a codec switch rescales.
	#[test]
	fn native_and_codec_changes_rebuild_the_ladder() {
		let mut c = Config::new(VCodec::H264, 30_000);
		c.ladder = true;
		c.native = (1920, 1200, 120);
		let mut c = Controller::new(c);
		let h264_top = c.points()[0].min_kbps;
		c.set_native(2560, 1440, 120);
		assert_eq!((c.points()[0].width, c.points()[0].height), (2560, 1440), "{:?}", c.points()[0]);
		c.set_native(1920, 1200, 120);
		c.set_codec(VCodec::H265);
		assert!(c.points()[0].min_kbps < h264_top, "HEVC needs fewer bits");
	}

	#[test]
	fn host_bound_step_respects_a_pinned_point() {
		let mut cfg = cfg(15_000, true);
		cfg.native = (1920, 1080, 120);
		cfg.ladder = false;
		let mut c = Controller::new(cfg);
		let mut sim = HostSim::new();
		for _ in 0..20 {
			for d in sim.window(&mut c, 1920 * 1080 * 60) {
				assert!(d.point.is_none(), "{d:?}");
			}
		}
		assert_eq!(c.point().label(), "1080p120");
	}

	/// A measured window (goodput reported): `recv` packets of 1100 bytes in 2 s, so the
	/// goodput is `recv × 4.4` kbit/s.
	fn mwin(recv: u32, lost: u32, rtt: f32) -> Sample {
		Sample { recv, lost, rtt_ms: vec![rtt; 4], recv_bytes: recv as u64 * 1100, window_ms: 2000, ..Default::default() }
	}

	/// Packets the encoder rate puts on the wire in a 2 s window (1100-byte packets).
	fn pkts(c: &Controller) -> u32 {
		c.encoder_kbps() * 2000 / 8 / 1100
	}

	#[test]
	fn no_memory_starts_cautiously_under_the_cap() {
		assert_eq!(Controller::new(Config::new(VCodec::H264, 15_000)).target_kbps(), START_KBPS_NO_MEMORY);
		assert_eq!(Controller::new(Config::new(VCodec::H264, 2_500)).target_kbps(), 2_500, "never over the cap");
		let mut remembered = Config::new(VCodec::H264, 15_000);
		remembered.start_kbps = 9_000;
		assert_eq!(Controller::new(remembered).target_kbps(), 9_000, "a memory hint is used as-is");
	}

	/// ×1.5 per clean startup window (was ×2), and the ramp keeps going past `startup_windows`
	/// while it climbs cleanly: a clean 100 Mbit cap is reached in 9 windows from 4 Mbit.
	#[test]
	fn startup_ramp_climbs_x1_5_until_the_cap_while_clean() {
		let mut c = Controller::new(Config::new(VCodec::H265, 100_000));
		let mut seen = vec![c.target_kbps()];
		for _ in 0..9 {
			c.tick(&win(3000, 0, 2.0));
			seen.push(c.target_kbps());
		}
		assert_eq!(&seen[..4], &[4_000, 6_000, 9_000, 13_500], "{seen:?}");
		assert_eq!(c.target_kbps(), 100_000, "{seen:?}");
	}

	/// One mild-loss window only pauses the startup ramp (the opening IDR burst, a Wi-Fi
	/// retry); the second congestion sign ends it. A queue ends it at once.
	#[test]
	fn startup_ramp_pauses_once_and_settles_at_the_second_congestion_sign() {
		let mut c = Controller::new(Config::new(VCodec::H265, 100_000));
		c.tick(&win(2000, 0, 20.0));
		assert_eq!(c.target_kbps(), 6_000);
		c.tick(&win(1980, 20, 20.0)); // 1 % loss: not a cut, but not clean either
		assert_eq!(c.target_kbps(), 6_000);
		c.tick(&win(2000, 0, 20.0));
		assert_eq!(c.target_kbps(), 9_000, "the ramp only paused");
		c.tick(&win(1980, 20, 20.0));
		assert_eq!(c.target_kbps(), 9_000);
		c.tick(&win(2000, 0, 20.0));
		assert_eq!(c.target_kbps(), 9_000, "no longer the per-window startup climb");
		c.tick(&win(2000, 0, 20.0));
		assert_eq!(c.target_kbps(), 13_500, "the far-under-the-ceiling pace (×1.5 / 2 windows)");
	}

	/// A clean LAN whose window 1 loses 1 % (an IDR burst) still reaches the cap within ~12
	/// windows — it took 21 when that one window ended the ramp at 4 Mbit.
	#[test]
	fn one_early_lossy_window_does_not_cost_a_lan_its_ramp() {
		let mut cfg = Config::new(VCodec::H265, 100_000);
		cfg.native = (1920, 1080, 120);
		cfg.ladder = true;
		let mut c = Controller::new(cfg);
		let mut at = None;
		for w in 0..20u32 {
			let pk = pkts(&c);
			let lost = if w == 1 { pk / 100 } else { 0 };
			c.tick(&mwin(pk - lost, lost, 2.0));
			if at.is_none() && c.target_kbps() >= 95_000 {
				at = Some(w);
			}
		}
		assert!(matches!(at, Some(w) if w <= 12), "cap at {at:?} ({})", c.target_kbps());
	}

	/// A probe-down over a learned 35 % floor is judged on the loss OVER the floor: a rate
	/// over a policer that drew +7 % over it, gone at the lower rate, is kept lower (ceiling
	/// punished) — raw loss can never fall to 60 % of 42 %, so it looped forever.
	#[test]
	fn probe_down_over_a_noise_floor_is_judged_over_the_floor() {
		let mut c = Controller::new(cfg(15_000, false));
		c.noise_loss = 0.35;
		c.noise_ok_rate = 9_000;
		c.startup_left = 0;
		c.ceiling = 15_000;
		c.target = 11_250;
		let over = |c: &Controller| {
			let total = c.encoder_kbps() * 2000 / 8 / 1100;
			let lost = total * 423 / 1000;
			mwin(total - lost, lost, 78.0)
		};
		let at_floor = |c: &Controller| {
			let total = c.encoder_kbps() * 2000 / 8 / 1100;
			let lost = total * 35 / 100;
			mwin(total - lost, lost, 78.0)
		};
		let mut d = Decision::default();
		for _ in 0..4 {
			let s = over(&c);
			d = c.tick(&s);
			if c.probe_down.is_some() {
				break;
			}
		}
		assert!(c.probe_down.is_some(), "{d:?}");
		assert_eq!(c.target_kbps(), 9_000);
		for _ in 0..2 {
			let s = at_floor(&c);
			d = c.tick(&s);
		}
		assert_eq!(d.reason, "sustained loss followed the rate — kept the lower rate");
		assert_eq!(c.target_kbps(), 9_000);
		assert!(c.ceiling() < 11_250, "ceiling punished: {}", c.ceiling());
		assert!(c.noise_loss() <= 0.351, "no ratchet: {}", c.noise_loss());
	}

	/// The same probe on loss that did NOT follow the rate (random): restored, and the floor
	/// learns at most what the probe's start showed.
	#[test]
	fn probe_down_that_did_not_follow_restores_without_a_ratchet() {
		let mut c = Controller::new(cfg(15_000, false));
		c.noise_loss = 0.30;
		c.startup_left = 0;
		c.ceiling = 15_000;
		c.target = 9_000;
		let lossy = |c: &Controller, permille: u32| {
			let total = c.encoder_kbps() * 2000 / 8 / 1100;
			let lost = total * permille / 1000;
			mwin(total - lost, lost, 78.0)
		};
		let mut d = Decision::default();
		for _ in 0..4 {
			let s = lossy(&c, 350);
			d = c.tick(&s);
			if c.probe_down.is_some() {
				break;
			}
		}
		assert!(c.probe_down.is_some(), "{d:?}");
		for _ in 0..2 {
			let s = lossy(&c, 345);
			d = c.tick(&s);
		}
		assert_eq!(d.reason, "sustained loss did not follow the rate — learned as noise, restored");
		assert_eq!(c.target_kbps(), 9_000);
		assert!((0.344..=0.351).contains(&c.noise_loss()), "{}", c.noise_loss());
	}

	/// A window of the controller's own rate with `raw_pm` ‰ of its packets dropped before
	/// repair, `post_pm` ‰ still missing (the rest NACK-repaired); `window_ms` 0 = unmeasured.
	fn rwin(c: &Controller, raw_pm: u32, post_pm: u32, window_ms: u32) -> Sample {
		let total = pkts(c);
		let lost = total * post_pm / 1000;
		let repaired = (total * raw_pm / 1000).saturating_sub(lost);
		let recv = total - lost;
		let recv_bytes = if window_ms > 0 { recv as u64 * 1100 } else { 0 };
		Sample { recv, lost, rtt_ms: vec![78.0; 4], nack_sent: repaired, nack_ok: repaired, recv_bytes, window_ms, ..Default::default() }
	}

	/// The pol10 field case: at the ceiling, 12 % pre-repair loss (4 % after NACK) while the
	/// host's parity climbs 20 → 50 %; the probe-down cures the pre-repair loss. Measured, that
	/// is a loss episode — the lower rate stays, the ceiling drops to ~0.9 × the failed rate and
	/// the climb back creeps up in small steps without reaching it again. Unmeasured (mobile)
	/// keeps the old verdict on post-repair loss: the parity change makes it inconclusive.
	#[test]
	fn a_probe_down_that_cured_the_raw_loss_lowers_the_ceiling_and_stays_down() {
		let mut c = Controller::new(cfg(15_000, false));
		c.startup_left = 0;
		c.set_target(10_227);
		c.ceiling = 10_227;
		c.set_fec_overhead(0.20);
		for _ in 0..4 {
			let s = rwin(&c, 10, 0, 2000);
			c.tick(&s);
		}
		assert_eq!(c.target_kbps(), 10_227);
		let mut d = Decision::default();
		for fec in [0.25, 0.40] {
			let s = rwin(&c, 120, 40, 2000);
			c.set_fec_overhead(fec);
			d = c.tick(&s);
		}
		assert_eq!(d.reason, "sustained loss, flat link → probe down", "{d:?}");
		assert!((0.11..0.13).contains(&c.last().raw_loss) && c.last().loss < 0.05, "{:?}", c.last());
		let low = c.target_kbps();
		assert_eq!(low, 10_227 * 4 / 5);
		for fec in [0.50, 0.57] {
			let s = rwin(&c, 5, 0, 2000);
			c.set_fec_overhead(fec);
			d = c.tick(&s);
		}
		assert_eq!(d.reason, "sustained loss followed the rate — kept the lower rate", "{d:?}");
		assert_eq!(c.target_kbps(), low, "not restored");
		assert_eq!(c.ceiling(), 10_227 * 9 / 10, "~0.9 × the failed rate");
		// The climb back: small steps, never the failed rate again (the field went straight back
		// to 10 227 after one clean window, and lost it again 2 s later).
		let mut prev = c.target_kbps();
		for _ in 0..40 {
			let s = rwin(&c, 10, 0, 2000);
			c.tick(&s);
			let t = c.target_kbps();
			assert!(t <= prev * 109 / 100, "{prev} → {t} in one step");
			assert!(t < 10_227, "back at the failed rate: {t}");
			prev = t;
		}
		assert_eq!(c.target_kbps(), c.ceiling(), "climbed back to the new ceiling");

		// Unmeasured samples: the post-repair verdict, as before.
		let mut c = Controller::new(cfg(15_000, false));
		c.startup_left = 0;
		c.set_target(10_227);
		c.ceiling = 10_227;
		c.set_fec_overhead(0.20);
		for _ in 0..4 {
			let s = rwin(&c, 10, 0, 0);
			c.tick(&s);
		}
		for fec in [0.25, 0.40] {
			let s = rwin(&c, 120, 40, 0);
			c.set_fec_overhead(fec);
			c.tick(&s);
		}
		assert_eq!(c.target_kbps(), 10_227 * 4 / 5);
		for fec in [0.50, 0.57] {
			let s = rwin(&c, 5, 0, 0);
			c.set_fec_overhead(fec);
			d = c.tick(&s);
		}
		assert_eq!(d.reason, "probe down inconclusive (fec changed) — restored", "{d:?}");
		assert_eq!(c.target_kbps(), 10_227);
	}

	/// The clean threshold scales with the floor's binomial scatter: 1.5 % over a 35 % floor
	/// on ~900 packets is ordinary noise (counts toward a climb); with no floor it is not.
	#[test]
	fn clean_tolerance_scales_with_the_noise_floor() {
		let mut c = Controller::new(cfg(15_000, false));
		assert_eq!(c.loss_tolerance(900), LOSS_CLEAN, "no floor: as before");
		c.noise_loss = 0.35;
		let tol = c.loss_tolerance(900);
		assert!(tol > 0.02 && tol <= LOSS_DOWN, "≈ 2σ = 3.2 %, capped at LOSS_DOWN: {tol}");
		assert!(c.loss_tolerance(100_000) < 0.005 + 1e-6, "many packets: tight again");
		// A window 1.5 % over the floor climbs.
		c.startup_left = 0;
		c.ceiling = 15_000;
		c.target = 4_000;
		let mut climbed = false;
		for _ in 0..3 {
			let total = 900u32;
			let lost = total * 365 / 1000;
			if c.tick(&mwin(total - lost, lost, 78.0)).bitrate.is_some() {
				climbed = true;
			}
		}
		assert!(climbed, "target {}", c.target_kbps());
	}

	/// Measured severe loss over a learned random floor: the delivered rate is de-noised
	/// (÷ (1 − floor)) before the β and the ceiling — a 10 Mbit path with a 35 % floor
	/// delivers 6.5 Mbit, and a ceiling there sat at 70 % of the capacity for minutes.
	#[test]
	fn measured_severe_cut_de_noises_the_delivered_rate() {
		let mut c = Controller::new(cfg(15_000, false));
		c.noise_loss = 0.35;
		c.startup_left = 0;
		c.ceiling = 15_000;
		c.target = 14_000;
		c.lossy = 2;
		// 14 Mbit offered, 10 Mbit carried, 35 % of that dropped: 6.5 Mbit arrive (53.6 % loss).
		let total = 14_000 * 2000 / 8 / 1100;
		let recv = 6_500 * 2000 / 8 / 1100;
		let d = c.tick(&mwin(recv, total - recv, 78.0));
		assert!(d.reason.starts_with("severe loss"), "{d:?}");
		assert!(c.ceiling() >= 9_500, "ceiling at what the bottleneck carried: {}", c.ceiling());
		assert!((8_000..=10_000).contains(&c.target_kbps()), "0.85 × ~10 Mbit: {}", c.target_kbps());
	}

	/// Measured severe loss: the cut goes to 0.85 × what the path delivered, not a halving —
	/// and the path delivering clearly under the point's minimum steps the point at once.
	#[test]
	fn measured_severe_loss_cuts_to_0_85_of_the_delivered_rate() {
		let mut c = Controller::new(cfg(10_000, false));
		assert_eq!(c.point().label(), "1080p60");
		for _ in 0..3 {
			c.tick(&mwin(2272, 0, 20.0));
		}
		let d = c.tick(&mwin(1364, 454, 20.0)); // 25 % loss, 6 Mbit delivered
		assert!((5_090..=5_110).contains(&c.target_kbps()), "{} {d:?}", c.target_kbps());
		assert_eq!(c.point().label(), "720p60", "6 Mbit delivered < 1080p60's 8 Mbit minimum");
		assert!(d.point.is_some() && d.bitrate.is_some(), "{d:?}");
		assert!(c.last().goodput_kbps > 5_900.0 && c.last().send_kbps > 7_900.0, "{:?}", c.last());
	}

	/// One bad window doesn't cost the picked point: the rate is held at the point's minimum
	/// and the point stays; a second one in a row steps it.
	#[test]
	fn a_single_lossy_window_holds_the_point_at_its_floor() {
		let mut c = cfg(12_000, false);
		c.codec = VCodec::H264;
		let mut c = Controller::new(c);
		c.set_pins(Some((1920, 1080)), Some(60));
		assert_eq!(c.point().label(), "1080p60");
		for _ in 0..8 {
			c.tick(&mwin(2727, 0, 20.0));
		}
		// 30 % loss, 8.4 Mbit delivered: the cut wants ~7.1 Mbit, under 1080p60's 8 Mbit.
		let d = c.tick(&mwin(1909, 818, 20.0));
		assert_eq!(c.point().label(), "1080p60", "{d:?}");
		assert_eq!(c.target_kbps(), 8_000, "held at the point's minimum");
		assert!(c.pick_shortfall().is_none(), "still at the pick");
		// The path holds the floor: no step.
		for _ in 0..3 {
			c.tick(&mwin(pkts(&c), 0, 20.0));
		}
		assert_eq!(c.point().label(), "1080p60");
		// Sustained: a 7.6 Mbit bottleneck under the 8 Mbit minimum (delivering more than 85 %
		// of it, so no at-once step) — the mild-loss cuts keep wanting less for 2 windows.
		let cap_pkts = 7_600 * 2000 / 8 / 1100;
		let mut stepped = None;
		for w in 0..10 {
			let p = pkts(&c);
			let recv = p.min(cap_pkts);
			let d = c.tick(&mwin(recv, p - recv, 20.0));
			if d.point.is_some() {
				stepped = Some(w);
				break;
			}
		}
		assert!(matches!(stepped, Some(w) if w >= 2), "a sustained shortfall steps the point: {stepped:?}");
		assert!(c.point().fps < 60 || c.point().height < 1080);
		assert_eq!(c.pick_shortfall().map(|(p, _)| p.label()), Some("1080p60".to_string()));
	}

	/// Random loss that doesn't follow the rate: judged non-congestive after one cut, the
	/// cut undone; the flag clears once the loss goes away.
	#[test]
	fn non_responsive_loss_is_learned_and_the_cut_undone() {
		let mut c = Controller::new(cfg(8_000, false));
		for _ in 0..3 {
			c.tick(&mwin(1818, 0, 20.0));
		}
		let mut reasons = Vec::new();
		for _ in 0..10 {
			let p = pkts(&c);
			let d = c.tick(&mwin(p * 7 / 10, p * 3 / 10, 20.0));
			reasons.push(d.reason);
		}
		assert!(c.loss_is_non_responsive(), "{reasons:?}");
		assert!(reasons.iter().filter(|r| r.contains("0.85 × delivered")).count() == 1, "one cut: {reasons:?}");
		assert!(c.target_kbps() >= 7_000, "restored: {} {reasons:?}", c.target_kbps());
		for _ in 0..3 {
			c.tick(&mwin(1818, 0, 20.0));
		}
		assert!(!c.loss_is_non_responsive());
	}

	/// The host sending far over its rate (keyframes, parity) under severe loss: a lighter
	/// point, not a lower target.
	#[test]
	fn host_overshoot_steps_the_point_instead_of_cutting() {
		let mut c = Controller::new(cfg(4_000, false));
		assert_eq!(c.point().label(), "720p30");
		for _ in 0..3 {
			c.tick(&mwin(909, 0, 20.0));
		}
		// 6.4 Mbit arrived at 20 % loss: the host sent ~8 Mbit against a 4 Mbit request.
		let d = c.tick(&mwin(1455, 364, 20.0));
		assert_eq!(d.point.map(|p| p.label()), Some("540p30".to_string()), "{d:?}");
		assert_eq!(c.target_kbps(), 4_000, "{d:?}");
	}

	/// The fast reflex's cut is the window's cut: the tick that closes the window holds.
	#[test]
	fn the_fast_reflex_counts_as_the_windows_cut() {
		let mut c = Controller::new(cfg(8000, true));
		for i in 0..60 {
			c.on_frame((i as f64 * 16.667 * 90.0) as u32, i as f64 * 16.667 + 20.0);
		}
		let mut cut = false;
		for i in 60..200 {
			let t = i as f64 * 16.667;
			c.on_frame((t * 90.0) as u32, t + 20.0 + (i - 60) as f64 * 1.5);
			if c.poll_fast(t).is_some() {
				cut = true;
				break;
			}
		}
		assert!(cut);
		let after_fast = c.target_kbps();
		let d = c.tick(&win(1000, 0, 20.0));
		assert_eq!(c.target_kbps(), after_fast, "{d:?}");
		assert!(d.reason.contains("already cut"), "{d:?}");
	}

	/// The fps-aware floor holds a fixed point too: 1080p120 never gets under ~1.5 KB a frame.
	#[test]
	fn a_fixed_high_fps_point_keeps_the_fps_aware_floor() {
		let mut m = Config::new(VCodec::H264, 8_000);
		m.native = (1920, 1080, 120);
		let mut c = Controller::new(m);
		// (Short of `FLOOR_ESCAPE_WINDOWS` at the floor: the escape probe would lift it.)
		for _ in 0..8 {
			c.tick(&win(500, 500, 20.0));
			assert!(c.target_kbps() >= rate_floor_for_fps(120), "{}", c.target_kbps());
		}
		assert_eq!(c.point().label(), "1080p120");
		assert_eq!(c.target_kbps(), rate_floor_for_fps(120));
		assert_eq!(rate_floor_for_fps(120), 1_440);
	}

	/// `pick_shortfall`: nothing without pins or during the startup climb; the pick and the
	/// current point once the link holds the session under the pick.
	#[test]
	fn pick_shortfall_reports_a_lighter_point_than_the_pick() {
		let mut g = Config::new(VCodec::H265, 20_000);
		g.ladder = true;
		g.native = (1920, 1080, 120);
		g.min_fps = 60;
		g.min_height = 720;
		let mut c = Controller::new(g.clone());
		c.set_user_cap(3_000);
		assert!(c.pick_shortfall().is_none(), "no pins");
		c.set_pins(Some((1920, 1080)), Some(120));
		assert_eq!(c.points()[0].label(), "1080p120");
		assert_eq!(c.point().label(), "720p60", "3 Mbit buys the bottom rung only");
		assert!(c.pick_shortfall().is_none(), "still starting");
		for _ in 0..6 {
			c.tick(&win(700, 0, 20.0));
		}
		let (pick, cur) = c.pick_shortfall().expect("under the pick");
		assert_eq!((pick.label(), cur.label()), ("1080p120".to_string(), "720p60".to_string()));
		// Room for the pick again: back on it, no shortfall.
		c.set_user_cap(0);
		for _ in 0..60 {
			c.tick(&win(3000, 0, 20.0));
		}
		assert_eq!(c.point().label(), "1080p120");
		assert!(c.pick_shortfall().is_none());
		// Unpinned: never a shortfall.
		let mut c = Controller::new(g);
		c.set_user_cap(3_000);
		for _ in 0..6 {
			c.tick(&win(700, 0, 20.0));
		}
		assert!(c.pick_shortfall().is_none());
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
