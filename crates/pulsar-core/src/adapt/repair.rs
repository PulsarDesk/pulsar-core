//! The client's video repair loop, shared by the desktop (`pulsar-tauri` play/hold.rs) and the
//! mobile client (`pulsar-mobile` client.rs). Pure bookkeeping, no I/O: the apps feed it the
//! media datagrams of the session and send what it returns (NACK lists, keyframe requests),
//! and forward the packets FEC rebuilt to their depacketizer / renderer.
//!
//! * [`RepairTracker`] — video seq-gap tracking: every gap waits the reorder wait (and FEC's go
//!   while the host sends parity) before its NACK, is re-NACKed once ~1 RTT later, and is
//!   written off after the RTT-derived repair window. Counts the window's loss / repairs /
//!   goodput for the adaptive [`Controller`], and learns the host's FEC share from the parity it
//!   receives (released again when the parity stops).
//! * [`RttWindow`] — the keepalive RTT samples: median + jitter size the repair window.
//! * [`KeyframePacer`] — keyframe requests back off during a loss streak.
//! * [`FrameClock`] — per-frame arrival times for the controller's delay-gradient estimator.
//! * free functions — the waits and windows themselves, unit-tested below.
//!
//! Extracted 2026-11-15 from the desktop hold loop without a behaviour change; the mobile
//! client used an older subset (instant NACK, no re-NACK, a 100-500 ms hold).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::controller::{Controller, Sample};
use super::fec_policy;
use crate::service::media;

/// How many recent RTT samples size the repair window (~4 s of Pings).
pub const RTT_RECENT: usize = 8;
/// How often unanswered NACKs are swept (re-NACKed once, then written off as a failed
/// repair → keyframe request + renderer hold). 50 ms (was 100): the renderer waits one sweep
/// longer than this loop (see [`RepairTracker::renderer_wait_ms`]), so the sweep period is extra
/// head-of-line wait behind every unrepaired hole; a 20 Hz pass over a few dozen entries costs
/// nothing.
pub const REPAIR_SWEEP_MS: u64 = 50;
/// Bounds of the RTT-derived repair window (ms): how long a missing packet is waited for
/// before the repair is declared failed. The renderer's RTP reorder `max_delay` is this plus
/// one sweep, so both give up together, and a hole holds the frames behind it for 250 ms at
/// most (the renderer's own cap) — a repair slower than that loses to the RFI / keyframe
/// recovery anyway. See [`repair_window_ms`].
pub const REPAIR_MIN_MS: f32 = 60.0;
pub const REPAIR_MAX_MS: f32 = 250.0 - REPAIR_SWEEP_MS as f32;
/// Allowance on top of the RTT for the host's side of a retransmit (its pacer queue + send).
pub const REPAIR_SLACK_MS: f32 = 20.0;
/// The longest a renderer / reorder buffer holds the frames behind a hole (ms):
/// `REPAIR_MAX_MS` plus one sweep.
pub const RENDERER_WAIT_MAX_MS: f32 = REPAIR_MAX_MS + REPAIR_SWEEP_MS as f32;

/// Game mode on a high-RTT path (median RTT at or over `FAST_DROP_RTT_ON_MS`; off again under
/// `FAST_DROP_RTT_OFF_MS`) with a recovery that needs no keyframe (RFI / intra refresh): a hole
/// is dropped fast instead of holding every later frame for a NACK repair that needs ≥ 1 RTT —
/// Moonlight-like (moonlight-common-c drops an incomplete frame as soon as the next one's
/// packets arrive and asks for RFI / IDR, no NACK at all). The renderer's reorder wait shrinks
/// to the reorder / FEC wait plus a frame (`fast_drop_wait_ms`) and no NACK is sent (its
/// answer would land after the renderer gave up). Remote-desktop sessions keep the
/// repair-friendly wait: a held frame there is text that arrives whole, not a stutter.
pub const FAST_DROP_RTT_ON_MS: f32 = 35.0;
pub const FAST_DROP_RTT_OFF_MS: f32 = 25.0;
/// Bounds of the fast-drop wait (ms): at least a couple of frames of reordering, at most what
/// a mobile link's reordering / FEC wait needs — never the 250 ms repair cap.
pub const FAST_DROP_MIN_MS: f32 = 30.0;
pub const FAST_DROP_MAX_MS: f32 = 80.0;

/// Whether the fast drop is on (`prev` = its last state; hysteresis on the median RTT).
pub fn fast_drop_on(prev: bool, game_mode: bool, keyframe_free: bool, rtt_med_ms: f32) -> bool {
	if !game_mode || !keyframe_free || !(rtt_med_ms > 0.0) {
		return false;
	}
	if prev {
		rtt_med_ms >= FAST_DROP_RTT_OFF_MS
	} else {
		rtt_med_ms >= FAST_DROP_RTT_ON_MS
	}
}

/// The fast-drop wait (ms): the gap's reorder / FEC wait (`nack_hold_ms`, see `nack_wait_ms`)
/// plus one frame interval and a little slack, bounded by `FAST_DROP_MIN_MS..=FAST_DROP_MAX_MS`.
pub fn fast_drop_wait_ms(nack_hold_ms: f32, fps: u32) -> f32 {
	let frame = 1000.0 / if fps == 0 { 60 } else { fps } as f32;
	(nack_hold_ms.max(0.0) + frame + 10.0).clamp(FAST_DROP_MIN_MS, FAST_DROP_MAX_MS)
}

/// Keyframe-request pacing. While losses keep coming, every request within 5 s of the previous
/// one doubles the gap (0.5 s → 1 → 2 → 4 s); 5 s without one resets it. And never sooner than
/// ~2 RTT: the keyframe asked for can't have arrived yet. Without this a lossy link fell into a
/// keyframe storm — 1080p keyframes every ~0.4 s were most of the traffic (2 Mbit/s sent for a
/// 0.3 Mbit/s target), which caused the next losses, which asked for more keyframes.
pub const KF_GAP_MIN: Duration = Duration::from_millis(500);
pub const KF_GAP_MAX: Duration = Duration::from_secs(4);

pub fn kf_allowed(since_last: Duration, gap: Duration, rtt_ms: f32) -> bool {
	since_last >= gap.max(Duration::from_secs_f32((2.0 * rtt_ms.max(0.0) + 100.0) / 1000.0))
}

pub fn kf_next_gap(since_last: Duration, gap: Duration) -> Duration {
	if since_last < Duration::from_secs(5) {
		(gap * 2).min(KF_GAP_MAX)
	} else {
		KF_GAP_MIN
	}
}

/// Median of the recent RTT samples (`fallback` while there are none).
pub fn median_ms(v: &VecDeque<f32>, fallback: f32) -> f32 {
	if v.is_empty() {
		return fallback;
	}
	let mut s: Vec<f32> = v.iter().copied().collect();
	s.sort_by(f32::total_cmp);
	s[s.len() / 2]
}

/// The client decoder cannot keep up with `fps`: each access unit takes most of the frame time
/// (`decode_ms`) AND fewer AUs get decoded than the stream sends (`decoded` per second; 0 = the
/// renderer does not report it). The time alone misleads: the Windows decode call returns when
/// the picture is out, so on a decoder that is merely pipelined it reads about one frame
/// interval at ANY fps — 11 ms at 120, 17 ms at 60 — and the old rule stepped 120 → 60 → 30
/// on a laptop whose decoder had room to spare (2026-11-14). A decoder that is really behind
/// sheds AUs (the backlog overflows), so its decoded rate falls below the stream's.
pub fn decoder_overloaded(decode_ms: f32, decoded: f32, fps: u32) -> bool {
	if !(decode_ms > 0.0) || !(decoded > 0.0) || fps == 0 {
		return false;
	}
	let budget = 1000.0 / fps as f32;
	decode_ms >= budget * 0.8 && decoded < fps as f32 * 0.85
}

/// A decoder that sheds what it is fed, measured over one window: `fed` access units went in,
/// `decoded` pictures came out, `dropped` AUs were thrown away by its input backlog guard. Behind
/// = it dropped any, or it put out under 85 % of what it was fed (a few AUs of pipeline depth
/// are noise over a 2 s window). For decoders that report no decode time (Android MediaCodec):
/// what they DO show is the backlog. Too few AUs fed (< 10, a static screen) says nothing.
pub fn decoder_sheds(fed: u64, decoded: u64, dropped: u64) -> bool {
	if fed < 10 {
		return false;
	}
	dropped > 0 || (decoded as f64) < fed as f64 * 0.85
}

/// The repair window in ms (see `REPAIR_MIN_MS`): a NACK retransmit lands ~1 RTT after the
/// NACK, so wait the median RTT plus twice its jitter (the mean absolute deviation of the
/// recent samples) — an answer that is late but normal for this link still makes it, a lost one
/// is not waited out for long. `fec_hold_ms` = how long NACKs are held back for FEC now (see
/// `fec_wait_ms`). The old `1.5 × RTT + 100` (+ a 100 ms sweep) reached ~600 ms on a jittery
/// mobile link, every frame behind a hole held that long. No samples yet: the upper bound.
pub fn repair_window_ms(rtt_recent: &VecDeque<f32>, fec_hold_ms: f32) -> f32 {
	if rtt_recent.is_empty() {
		return REPAIR_MAX_MS;
	}
	let med = median_ms(rtt_recent, 0.0);
	let jitter = rtt_jitter_ms(rtt_recent);
	(med + 2.0 * jitter + REPAIR_SLACK_MS + fec_hold_ms.max(0.0)).clamp(REPAIR_MIN_MS, REPAIR_MAX_MS)
}

/// How much the round trip varies (ms): the mean absolute deviation of the recent RTT samples
/// around their median. Sizes the repair window, and is what the UI shows next to the latency
/// ("4 ms (±0.6 ms)"). No samples = 0.
pub fn rtt_jitter_ms(rtt_recent: &VecDeque<f32>) -> f32 {
	if rtt_recent.is_empty() {
		return 0.0;
	}
	let med = median_ms(rtt_recent, 0.0);
	rtt_recent.iter().map(|r| (r - med).abs()).sum::<f32>() / rtt_recent.len() as f32
}

/// A video packet seen missing (a seq gap) that has not arrived or been rebuilt yet.
#[derive(Clone, Copy, Debug)]
pub struct Miss {
	/// When the gap showed it missing (the repair window runs from here).
	pub t0: Instant,
	/// When it was NACKed (`None` = held back while FEC may still rebuild it).
	pub nacked: Option<Instant>,
	/// The NACK was re-sent once (the first NACK or its answer may itself have been lost).
	pub renacked: bool,
}

/// How many recently FEC-rebuilt seqs to remember (several frames' worth at 120 fps).
pub const FEC_REBUILT_KEEP: usize = 256;
/// Parity seen within this long = the host is sending FEC (it turns it off on a clean path).
pub const FEC_ACTIVE_FOR: Duration = Duration::from_secs(1);

/// Whether the host sends FEC parity now (`since_parity` = since the last parity datagram).
pub fn fec_active(since_parity: Option<Duration>) -> bool {
	since_parity.is_some_and(|d| d < FEC_ACTIVE_FOR)
}

/// How long a missing packet's NACK waits for FEC at most while the host sends parity (it is
/// sent sooner once its block's parity arrived without rebuilding it, `fec_block_gave_up`):
/// two frame intervals + 8 ms, as a small frame's parity trails it by up to two more frames
/// (`media::RS_GROUP_MAX_FRAMES`) plus the pacing. A NACK sent before FEC had its go asks for a
/// retransmit FEC was about to make redundant — more traffic on the very link losing packets.
/// fps 0 = the host default: assume 60.
pub fn fec_wait_ms(fps: u32) -> f32 {
	let fps = if fps == 0 { 60 } else { fps };
	(2000.0 / fps as f32 + 8.0).clamp(15.0, 50.0)
}

/// Bounds of the reorder wait (ms): how long a seq gap is given to fill by itself before it is
/// NACKed. Some paths reorder a lot — a Wi-Fi Windows host (MF/ffmpeg) to the Orange Pi showed
/// ~20 % of video packets late by a few ms (2026-11-09) — and an instant NACK on every gap made
/// each of them a retransmit (duplicates) and, worse, a "repair" the controller read as ~20 %
/// pre-repair loss on a clean LAN (it cut to 2 Mbit).
pub const REORDER_WAIT_MIN_MS: f32 = 3.0;
pub const REORDER_WAIT_MAX_MS: f32 = 25.0;

/// The reorder wait for the observed reorder lateness (a slowly decaying peak, ms).
pub fn reorder_wait_ms(reorder_peak_ms: f32) -> f32 {
	(reorder_peak_ms * 1.25).clamp(REORDER_WAIT_MIN_MS, REORDER_WAIT_MAX_MS)
}

/// How long a gap waits before its NACK: the reorder wait, and while the host sends parity
/// also FEC's go (`fec_wait_ms`).
pub fn nack_wait_ms(fec_on: bool, fps: u32, reorder_peak_ms: f32) -> f32 {
	let reorder = reorder_wait_ms(reorder_peak_ms);
	if fec_on {
		fec_wait_ms(fps).max(reorder)
	} else {
		reorder
	}
}

/// A missing seq arrived: was it the retransmit our NACK asked for (a repair), or the original
/// arriving late (reordering)? Never NACKed = reordering. NACKed less than half an RTT ago = the
/// original too — a retransmit can't be back that soon.
pub fn arrival_is_repair(nacked: Option<Instant>, now: Instant, rtt_ms: f32) -> bool {
	nacked.is_some_and(|t| now.duration_since(t).as_secs_f32() * 1000.0 >= 0.5 * rtt_ms.max(0.0))
}

/// After a Reed-Solomon parity shard (`idx` of `m`) of a block was applied: FEC can no longer
/// rebuild the block's still-missing packets — more are missing than the block has parity, or
/// its last parity shard is here (the host sends them in order).
pub fn fec_block_gave_up(missing_in_block: usize, m: u8, idx: u8) -> bool {
	missing_in_block > m as usize || idx.saturating_add(1) >= m
}

/// How many of the block `base..base+k` are missing.
pub fn missing_in(missing: &HashMap<u16, Miss>, base: u16, k: u16) -> usize {
	(0..k).filter(|&i| missing.contains_key(&base.wrapping_add(i))).count()
}

/// The block `base..base+k`'s missing packets whose NACK still waits for FEC: marked NACKed
/// (`now`) and returned, as FEC is done with them.
pub fn take_block_nacks(missing: &mut HashMap<u16, Miss>, base: u16, k: u16, now: Instant) -> Vec<u16> {
	let mut out = Vec::new();
	for i in 0..k {
		let seq = base.wrapping_add(i);
		if let Some(e) = missing.get_mut(&seq) {
			if e.nacked.is_none() {
				e.nacked = Some(now);
				out.push(seq);
			}
		}
	}
	out
}

/// The held-back NACKs (`pending`, detection order) whose FEC wait (`wait`) ran out: marked
/// NACKed and returned. Seqs repaired meanwhile, or already NACKed with their block, drop out.
pub fn take_due_fec_nacks(
	pending: &mut VecDeque<(u16, Instant)>,
	missing: &mut HashMap<u16, Miss>,
	wait: Duration,
	now: Instant,
) -> Vec<u16> {
	let mut out = Vec::new();
	while let Some(&(seq, t)) = pending.front() {
		if now.duration_since(t) < wait {
			break;
		}
		pending.pop_front();
		if let Some(e) = missing.get_mut(&seq) {
			if e.nacked.is_none() {
				e.nacked = Some(now);
				out.push(seq);
			}
		}
	}
	out
}

/// Remember a seq FEC rebuilt (bounded, oldest out).
pub fn note_rebuilt(rebuilt: &mut VecDeque<u16>, seq: u16) {
	if rebuilt.len() >= FEC_REBUILT_KEEP {
		rebuilt.pop_front();
	}
	rebuilt.push_back(seq);
}

/// Received video payload rate in kbit/s over a `window_ms` window (0 when unknown).
pub fn goodput_kbps(bytes: u64, window_ms: u32) -> u32 {
	if window_ms == 0 {
		0
	} else {
		(bytes * 8 / window_ms as u64).min(u32::MAX as u64) as u32
	}
}

/// The keepalive RTT readings of a session: the window's samples (for the controller), a
/// smoothed RTT, and the last few samples whose median and spread size the repair window (the
/// EMA keeps the first, setup-inflated ping for seconds).
#[derive(Clone, Debug, Default)]
pub struct RttWindow {
	recent: VecDeque<f32>,
	ema: f32,
	samples: Vec<f32>,
}

impl RttWindow {
	pub fn new() -> Self {
		Self { recent: VecDeque::with_capacity(RTT_RECENT), ema: 0.0, samples: Vec::with_capacity(8) }
	}

	/// One RTT reading (ms).
	pub fn on_sample(&mut self, rtt_ms: f32) {
		self.samples.push(rtt_ms);
		self.ema = if self.ema <= 0.0 { rtt_ms } else { self.ema * 0.7 + rtt_ms * 0.3 };
		if self.recent.len() >= RTT_RECENT {
			self.recent.pop_front();
		}
		self.recent.push_back(rtt_ms);
	}

	/// The last `RTT_RECENT` samples, oldest first.
	pub fn recent(&self) -> &VecDeque<f32> {
		&self.recent
	}

	/// The smoothed RTT (ms; 0 = no sample yet).
	pub fn ema(&self) -> f32 {
		self.ema
	}

	/// The median of the recent samples, `fallback` while there are none.
	pub fn median(&self, fallback: f32) -> f32 {
		median_ms(&self.recent, fallback)
	}

	/// See [`rtt_jitter_ms`].
	pub fn jitter(&self) -> f32 {
		rtt_jitter_ms(&self.recent)
	}

	/// The samples since the last call (one controller window's worth).
	pub fn take_samples(&mut self) -> Vec<f32> {
		std::mem::take(&mut self.samples)
	}
}

/// Keyframe-request pacing state (see `KF_GAP_MIN`): when the last request went out and the
/// current gap.
#[derive(Clone, Copy, Debug)]
pub struct KeyframePacer {
	last: Instant,
	gap: Duration,
}

impl KeyframePacer {
	/// `now` counts as the last request: the stream starts with a keyframe anyway.
	pub fn new(now: Instant) -> Self {
		Self { last: now, gap: KF_GAP_MIN }
	}

	/// Whether a keyframe may be asked for now (the gap, at least `floor`, and ~2 RTT since the
	/// last request); if so it counts as sent and the gap grows (or resets after a quiet 5 s).
	pub fn try_request(&mut self, now: Instant, floor: Duration, rtt_ms: f32) -> bool {
		let since = now.saturating_duration_since(self.last);
		if !kf_allowed(since, self.gap.max(floor), rtt_ms) {
			return false;
		}
		self.gap = kf_next_gap(since, self.gap);
		self.last = now;
		true
	}

	/// The current gap between requests.
	pub fn gap(&self) -> Duration {
		self.gap
	}
}

/// Per video FRAME (RTP timestamp), the arrival time of its last packet — what the
/// controller's delay-gradient estimator (`Controller::on_frame`) runs on.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameClock {
	ts: Option<u32>,
	last_arrival_ms: f64,
}

impl FrameClock {
	/// A video packet with RTP timestamp `ts` arrived at `now_ms` (a session-local ms clock).
	/// Returns the frame it closed — `(its timestamp, its last packet's arrival)` — when this is
	/// the first packet of a NEWER frame. A NACK-retransmitted (or reordered) packet of an older
	/// frame used to flip the frame back and re-sample frames against its late arrival — a steady
	/// fake "queue draining" signal that froze the bitrate climb.
	pub fn on_packet(&mut self, ts: u32, now_ms: f64) -> Option<(u32, f64)> {
		let newer = self.ts.is_none_or(|cur| (ts.wrapping_sub(cur) as i32) > 0);
		if newer {
			let closed = self.ts.map(|prev| (prev, self.last_arrival_ms));
			self.ts = Some(ts);
			self.last_arrival_ms = now_ms;
			closed
		} else {
			if self.ts == Some(ts) {
				self.last_arrival_ms = now_ms;
			}
			None
		}
	}

	/// A stream restart re-based the host's clock: start over.
	pub fn reset(&mut self) {
		self.ts = None;
	}
}

/// The RTP timestamp of a video RTP packet, if it has one.
pub fn rtp_timestamp(rtp: &[u8]) -> Option<u32> {
	(rtp.len() >= 8).then(|| u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]))
}

/// One window's measurement (see [`RepairTracker::take_window`]).
#[derive(Clone, Debug, Default)]
pub struct Window {
	/// Video packets received (every copy).
	pub recv: u32,
	/// Seq gaps that have not been filled (by the original, a retransmit or FEC) yet.
	pub lost: u32,
	/// NACKed seqs (re-NACKs included) and the retransmits that answered them.
	pub nack_sent: u32,
	pub nack_ok: u32,
	/// Missing seqs that arrived by themselves (reordering, not a repair).
	pub reordered: u32,
	/// Packets FEC rebuilt.
	pub fec_ok: u32,
	/// Video RTP payload received as FIRST copies (NACK-repaired ones included, duplicates and
	/// parity not): bytes and packets.
	pub recv_bytes: u64,
	pub recv_fresh: u32,
	/// Parity bytes received (RS + XOR).
	pub parity_bytes: u64,
	/// The keepalive RTT samples of the window.
	pub rtt_ms: Vec<f32>,
	/// The reorder wait at the end of the window (before the peak decays).
	pub reorder_wait_ms: f32,
}

impl Window {
	/// The controller's sample for this window (`window_ms` = its real length).
	pub fn sample(&self, window_ms: u32) -> Sample {
		Sample {
			recv: self.recv,
			lost: self.lost,
			rtt_ms: self.rtt_ms.clone(),
			nack_sent: self.nack_sent,
			nack_ok: self.nack_ok,
			fec_ok: self.fec_ok,
			recv_bytes: self.recv_bytes,
			window_ms,
		}
	}
}

/// What a video packet did (see [`RepairTracker::on_video`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoOutcome {
	/// NACKs to send now (empty while NACKs are off).
	pub nacks: Vec<u16>,
	/// The host's encoder restarted with a fresh RTP sequence base: the caller resyncs what it
	/// keeps per stream (`Controller::on_stream_restart`, its [`FrameClock`]).
	pub restart: bool,
}

/// What a parity datagram did.
#[derive(Clone, Debug, Default)]
pub struct ParityOutcome {
	/// The packets FEC rebuilt: hand them on as if they had arrived (late).
	pub rebuilt: Vec<(u16, Vec<u8>)>,
	/// NACKs to send now: what the block's parity could not rebuild.
	pub nacks: Vec<u16>,
}

/// What a repair sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
	/// Held-back NACKs whose wait ran out and re-NACKs to send now.
	pub nacks: Vec<u16>,
	/// Missing packets written off this sweep (a failed repair: the decoder's reference chain
	/// is broken until a keyframe, a reference-frame invalidation or an intra-refresh wave).
	pub expired: u32,
}

/// What [`RepairTracker::update_fec_share`] did to the controller's FEC share.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FecShare {
	/// Unchanged.
	Kept,
	/// No parity lately: the share went back to 0.
	Released,
	/// Set to this share.
	Set(f32),
}

/// The repair loop's state for one session's video stream: the gaps, the NACKs held back for
/// reordering / FEC, the FEC decoders, the window counters and the RTT.
pub struct RepairTracker {
	last_vseq: Option<u16>,
	/// Requested-but-not-yet-seen seqs (see `Miss`).
	missing: HashMap<u16, Miss>,
	/// Missing seqs whose NACK waits (reordering / FEC), detection order.
	fec_pending: VecDeque<(u16, Instant)>,
	/// The seqs FEC rebuilt lately (a block's tail can be rebuilt before the next frame's first
	/// packet shows the gap — not lost, not to be NACKed).
	fec_rebuilt: VecDeque<u16>,
	/// When the last parity datagram arrived.
	last_parity_at: Option<Instant>,
	/// The reorder lateness peak (ms, decays slowly) the reorder wait follows.
	reorder_peak_ms: f32,
	fec_xor: media::FecDecoder,
	fec_rs: media::RsFecDecoder,
	/// The smoothed parity share (see `update_fec_share`).
	fec_share_ema: f32,
	win: Window,
	rtt: RttWindow,
}

impl Default for RepairTracker {
	fn default() -> Self {
		Self::new()
	}
}

impl RepairTracker {
	pub fn new() -> Self {
		Self {
			last_vseq: None,
			missing: HashMap::new(),
			fec_pending: VecDeque::new(),
			fec_rebuilt: VecDeque::with_capacity(FEC_REBUILT_KEEP),
			last_parity_at: None,
			reorder_peak_ms: 0.0,
			fec_xor: media::FecDecoder::default(),
			fec_rs: media::RsFecDecoder::default(),
			fec_share_ema: 0.0,
			win: Window::default(),
			rtt: RttWindow::new(),
		}
	}

	/// A keepalive RTT reading (ms).
	pub fn on_rtt(&mut self, rtt_ms: f32) {
		self.rtt.on_sample(rtt_ms);
	}

	pub fn rtt(&self) -> &RttWindow {
		&self.rtt
	}

	/// Whether the host sends FEC parity now.
	pub fn fec_on(&self, now: Instant) -> bool {
		fec_active(self.last_parity_at.map(|t| now.saturating_duration_since(t)))
	}

	/// How long NACKs are held back for FEC now (ms; 0 while no parity flows).
	pub fn fec_hold_ms(&self, now: Instant, fps: u32) -> f32 {
		if self.fec_on(now) {
			fec_wait_ms(fps)
		} else {
			0.0
		}
	}

	/// How long a gap waits before its NACK now (ms; see `nack_wait_ms`).
	pub fn nack_hold_ms(&self, now: Instant, fps: u32) -> f32 {
		nack_wait_ms(self.fec_on(now), fps, self.reorder_peak_ms)
	}

	/// The reorder wait now (ms).
	pub fn reorder_wait_ms(&self) -> f32 {
		reorder_wait_ms(self.reorder_peak_ms)
	}

	/// How long the renderer's reorder buffer should hold the frames behind a hole (ms): one
	/// sweep longer than the repair window, so the write-off (and its keyframe request / hold)
	/// reaches the renderer before it flushes past the hole and decodes the smear. Fast drop:
	/// the reorder / FEC wait plus a frame.
	pub fn renderer_wait_ms(&self, now: Instant, fps: u32, fast_drop: bool) -> f32 {
		if fast_drop {
			fast_drop_wait_ms(self.nack_hold_ms(now, fps), fps)
		} else {
			repair_window_ms(&self.rtt.recent, self.fec_hold_ms(now, fps)) + REPAIR_SWEEP_MS as f32
		}
	}

	/// How many seqs are missing now.
	pub fn missing_count(&self) -> usize {
		self.missing.len()
	}

	fn rebuilt(&mut self, seq: u16) {
		if self.missing.remove(&seq).is_some() {
			self.win.lost = self.win.lost.saturating_sub(1);
		}
		note_rebuilt(&mut self.fec_rebuilt, seq);
		self.win.fec_ok += 1;
	}

	fn nacks_out(&mut self, nacks: Vec<u16>, nack_on: bool) -> Vec<u16> {
		if nack_on && !nacks.is_empty() {
			self.win.nack_sent += nacks.len() as u32;
			nacks
		} else {
			Vec::new()
		}
	}

	/// A Reed-Solomon parity datagram (`body` = after the media tag). Rebuilds what the block
	/// can — a repair with ZERO round-trips — and, once the block's parity is all here (or
	/// can't cover what is missing), NACKs the rest now: FEC is done with them.
	pub fn on_rs_parity(&mut self, body: &[u8], now: Instant, nack_on: bool) -> ParityOutcome {
		self.last_parity_at = Some(now);
		self.win.parity_bytes += body.len() as u64;
		let block = media::parse_rs_parity(body).map(|p| (p.base_seq, p.k as u16, p.m, p.idx));
		let rebuilt = self.fec_rs.on_parity(body);
		for (seq, _) in &rebuilt {
			self.rebuilt(*seq);
		}
		let mut nacks = Vec::new();
		if let Some((base, k, m, idx)) = block {
			if fec_block_gave_up(missing_in(&self.missing, base, k), m, idx) {
				nacks = take_block_nacks(&mut self.missing, base, k, now);
			}
		}
		let nacks = self.nacks_out(nacks, nack_on);
		ParityOutcome { rebuilt, nacks }
	}

	/// An XOR parity datagram (older hosts; `body` = after the media tag): one parity per group,
	/// so what it didn't rebuild, it never will.
	pub fn on_xor_parity(&mut self, body: &[u8], now: Instant, nack_on: bool) -> ParityOutcome {
		self.last_parity_at = Some(now);
		self.win.parity_bytes += body.len() as u64;
		let group = media::parse_parity(body).map(|p| (p.base_seq, p.n as u16));
		let mut rebuilt = Vec::new();
		if let Some((seq, pkt)) = self.fec_xor.on_parity(body) {
			self.rebuilt(seq);
			rebuilt.push((seq, pkt));
		}
		let mut nacks = Vec::new();
		if let Some((base, n)) = group {
			nacks = take_block_nacks(&mut self.missing, base, n, now);
		}
		let nacks = self.nacks_out(nacks, nack_on);
		ParityOutcome { rebuilt, nacks }
	}

	/// Forget the stream (the host restarted its encoder with a fresh RTP sequence base).
	fn resync(&mut self, seq: u16) {
		self.missing.clear();
		self.fec_pending.clear();
		self.fec_rebuilt.clear();
		self.fec_xor.reset();
		self.fec_rs.reset();
		self.last_vseq = Some(seq);
	}

	/// A video RTP packet `seq` (`rtp` = the whole RTP datagram) arrived at `now`. Gap tracking:
	/// counts the loss for the controller and holds each freshly-missing seq back (the reorder
	/// wait, plus FEC's go while parity flows) before NACKing it. `fps` = the stream's current
	/// frame rate (sizes the FEC wait).
	pub fn on_video(&mut self, seq: u16, rtp: &[u8], now: Instant, nack_on: bool, fps: u32) -> VideoOutcome {
		let mut out = VideoOutcome::default();
		self.fec_xor.on_packet(seq, rtp);
		self.fec_rs.on_packet(seq, rtp);
		self.win.recv += 1;
		// First copy of this seq? Duplicates (and stale reorders of a seq already seen) clear it
		// below; they carry no new payload.
		let mut fresh = true;
		// The ORIGINAL of a packet FEC already rebuilt (Wi-Fi reordering: the parity beat the late
		// original): it was never lost — take it back out of the repairs, which the controller
		// reads as pre-repair loss. On a busy LAN Wi-Fi 15-17 % "loss" was this alone
		// (2026-11-13), and the controller cut 16.8 → 5.3 Mbit for it.
		if !self.missing.contains_key(&seq) {
			if let Some(i) = self.fec_rebuilt.iter().position(|&r| r == seq) {
				self.fec_rebuilt.remove(i);
				self.win.fec_ok = self.win.fec_ok.saturating_sub(1);
				self.win.reordered += 1;
			}
		}
		if let Some(e) = self.missing.remove(&seq) {
			// A requested retransmit or a late reorder arrived — not lost. Only the retransmit
			// counts as a repair (the controller reads repairs as pre-repair loss); a late
			// original feeds the reorder wait instead.
			self.win.lost = self.win.lost.saturating_sub(1);
			if arrival_is_repair(e.nacked, now, self.rtt.median(self.rtt.ema)) {
				self.win.nack_ok += 1;
			} else {
				self.win.reordered += 1;
				let late = now.saturating_duration_since(e.t0).as_secs_f32() * 1000.0;
				self.reorder_peak_ms = self.reorder_peak_ms.max(late);
			}
		} else if let Some(last) = self.last_vseq {
			let d = media::seq_forward(last, seq);
			if d == 0 {
				// Duplicate / retransmit of the last seq — ignore.
				fresh = false;
			} else if d < 0x8000 && d <= 128 {
				// Small forward gap: every missing seq waits a little first (the reorder wait,
				// plus FEC's go while parity flows); `take_due_fec_nacks` sends it after.
				for i in 1..d {
					let m = last.wrapping_add(i);
					if self.fec_rebuilt.contains(&m) {
						// FEC rebuilt it before this gap showed: not lost.
						continue;
					}
					self.missing.insert(m, Miss { t0: now, nacked: None, renacked: false });
					self.fec_pending.push_back((m, now));
					self.win.lost += 1;
				}
				self.last_vseq = Some(seq);
			} else if d < 0x8000 {
				// Big forward gap (d > 128 but < 0x8000) — encoder restarted with a fresh RTP
				// sequence base (every monitor/codec switch rebuilds it). Resync cleanly.
				self.resync(seq);
				out.restart = true;
			} else {
				// Backward jump (d >= 0x8000): how far behind seq is relative to last.
				let back = 0x10000u32 - d as u32;
				if back > 128 {
					// Large backward jump = the host's encoder RESTARTED with a new sequence base
					// that happens to be numerically lower (roughly half of random bases land
					// here). Skipping these left last_vseq STUCK, so every later packet re-tripped
					// the discontinuity and NACK-flooded the session. Resync instead.
					self.resync(seq);
					out.restart = true;
				} else {
					// A small backward distance (≤ 128 seqs behind) = an ordinary stale reorder or
					// duplicate that arrived late: no NACK, no loss. (A seq still awaited is in
					// `missing`, handled above — so this one was already seen or written off.)
					fresh = false;
				}
			}
		} else {
			self.last_vseq = Some(seq);
		}
		if fresh {
			self.win.recv_fresh += 1;
			self.win.recv_bytes += rtp.len() as u64;
		}
		// Held-back NACKs whose wait ran out: send them now.
		if !self.fec_pending.is_empty() {
			let wait = Duration::from_secs_f32(self.nack_hold_ms(now, fps) / 1000.0);
			let nacks = take_due_fec_nacks(&mut self.fec_pending, &mut self.missing, wait, now);
			out.nacks = self.nacks_out(nacks, nack_on);
		}
		out
	}

	/// The periodic repair sweep (every `REPAIR_SWEEP_MS`): sends the held-back NACKs whose wait
	/// ran out while no packet came to send them (all of them once the parity stopped — nothing
	/// will rebuild them), re-NACKs once ~1 RTT after a NACK (the NACK or its answer may itself
	/// have been lost), and writes off what the repair window (or, `fast_drop`, the renderer's
	/// short wait) gave up on.
	pub fn sweep(&mut self, now: Instant, fps: u32, fast_drop: bool, nack_on: bool) -> SweepOutcome {
		let mut out = SweepOutcome::default();
		if self.missing.is_empty() {
			return out;
		}
		let nack_hold_ms = self.nack_hold_ms(now, fps);
		let wait = Duration::from_secs_f32(nack_hold_ms / 1000.0);
		let mut renack = take_due_fec_nacks(&mut self.fec_pending, &mut self.missing, wait, now);
		// Fast drop: written off when the renderer gives up on it (no repair is coming).
		let window_ms =
			if fast_drop { fast_drop_wait_ms(nack_hold_ms, fps) } else { repair_window_ms(&self.rtt.recent, nack_hold_ms) };
		let window = Duration::from_secs_f32(window_ms / 1000.0);
		let rtt_med = self.rtt.median(self.rtt.ema);
		let renack_after = Duration::from_secs_f32((rtt_med * 1.2 + 20.0).max(40.0) / 1000.0);
		let mut expired = 0u32;
		self.missing.retain(|&seq, e| {
			let age = now.saturating_duration_since(e.t0);
			if age >= window {
				expired += 1;
				false
			} else {
				// ~1 RTT after the NACK went out (a NACK held for FEC left later than the gap
				// showed).
				if !e.renacked && e.nacked.is_some_and(|t| now.saturating_duration_since(t) >= renack_after) {
					e.renacked = true;
					renack.push(seq);
				}
				true
			}
		});
		out.nacks = self.nacks_out(renack, nack_on);
		out.expired = expired;
		out
	}

	/// Close the measurement window: its counters and RTT samples (reset for the next one), and
	/// the reorder wait as it stood. The reorder peak decays so one burst of reordering doesn't
	/// slow every later NACK.
	pub fn take_window(&mut self) -> Window {
		let mut w = std::mem::take(&mut self.win);
		w.rtt_ms = self.rtt.take_samples();
		w.reorder_wait_ms = reorder_wait_ms(self.reorder_peak_ms);
		self.reorder_peak_ms *= 0.8;
		w
	}

	/// Keep the controller's FEC share (what it deducts from the target for the encoder) in step
	/// with the parity the host sends, from a closed window `w`. Measured over the window, not
	/// taken from the last block — at a low bitrate a block is 1-2 packets and its own ratio read
	/// 50-100 %, so the encoder share flipped 0 ↔ 100 % every window (2026-11-13). The share is
	/// learned from parity packets as they arrive, so a host that turns parity OFF sends nothing
	/// that would say so: without the release the controller kept deducting the last share for
	/// the rest of the session (2026-11-09 LAN run: a startup WAN guess turned 15 % parity on for
	/// one window, the host turned it off, and the encoder sat at 12 of the 15 Mbit target).
	pub fn update_fec_share(&mut self, ctl: &mut Controller, now: Instant, w: &Window) -> FecShare {
		if !self.fec_on(now) && ctl.fec_overhead() > 0.0 {
			ctl.set_fec_overhead(0.0);
			self.fec_share_ema = 0.0;
			return FecShare::Released;
		}
		if w.recv_bytes > 0 && w.parity_bytes > 0 {
			// This window's parity share, smoothed (half the old value, half the new) and capped
			// at the host's highest policy ratio, so one odd window can't halve the encoder.
			let share = (w.parity_bytes as f32 / w.recv_bytes as f32).min(fec_policy::RS_MAX_RATIO_HIGH);
			self.fec_share_ema = if self.fec_share_ema <= 0.0 { share } else { (self.fec_share_ema + share) * 0.5 };
			if (self.fec_share_ema - ctl.fec_overhead()).abs() > 0.01 {
				ctl.set_fec_overhead(self.fec_share_ema);
				return FecShare::Set(self.fec_share_ema);
			}
		}
		FecShare::Kept
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ms(v: u64) -> Duration {
		Duration::from_millis(v)
	}

	fn rtts(v: &[f32]) -> VecDeque<f32> {
		v.iter().copied().collect()
	}

	/// An RTP packet with `seq` / `ts` and a payload of `len` bytes.
	fn rtp(seq: u16, ts: u32, len: usize) -> Vec<u8> {
		let mut p = vec![0x80, 96];
		p.extend_from_slice(&seq.to_be_bytes());
		p.extend_from_slice(&ts.to_be_bytes());
		p.extend_from_slice(&[0, 0, 0, 1]);
		p.resize(12 + len, 0xab);
		p
	}

	#[test]
	fn keyframe_requests_back_off_during_a_loss_streak() {
		assert!(kf_allowed(ms(600), KF_GAP_MIN, 20.0));
		// Never sooner than ~2 RTT.
		assert!(!kf_allowed(ms(600), KF_GAP_MIN, 300.0));
		let mut gap = KF_GAP_MIN;
		for _ in 0..5 {
			gap = kf_next_gap(ms(600), gap);
		}
		assert_eq!(gap, KF_GAP_MAX);
		assert!(!kf_allowed(ms(3000), gap, 20.0));
		// A quiet 5 s resets it.
		assert_eq!(kf_next_gap(Duration::from_secs(6), gap), KF_GAP_MIN);
	}

	#[test]
	fn the_pacer_applies_the_gap_and_a_floor() {
		let t0 = Instant::now();
		let mut p = KeyframePacer::new(t0);
		assert!(!p.try_request(t0 + ms(300), Duration::ZERO, 10.0), "the stream just started");
		assert!(p.try_request(t0 + ms(600), Duration::ZERO, 10.0));
		assert_eq!(p.gap(), ms(1000), "a request within 5 s doubles the gap");
		assert!(!p.try_request(t0 + ms(1400), Duration::ZERO, 10.0));
		assert!(p.try_request(t0 + ms(1700), Duration::ZERO, 10.0));
		// The floor: a write-off asks at most once a second even with a small gap.
		let mut q = KeyframePacer::new(t0);
		assert!(!q.try_request(t0 + ms(700), Duration::from_secs(1), 10.0));
		assert!(q.try_request(t0 + ms(1000), Duration::from_secs(1), 10.0));
		// ~2 RTT on a slow path.
		let mut r = KeyframePacer::new(t0);
		assert!(!r.try_request(t0 + ms(600), Duration::ZERO, 400.0));
		assert!(r.try_request(t0 + ms(900), Duration::ZERO, 400.0));
	}

	#[test]
	fn a_pipelined_decoder_is_not_overloaded() {
		// ~one frame interval at any fps but every AU decoded: pipelined, not behind.
		assert!(!decoder_overloaded(11.0, 120.0, 120));
		assert!(!decoder_overloaded(17.0, 60.0, 60));
		// Behind: most of the budget AND shedding AUs.
		assert!(decoder_overloaded(12.0, 80.0, 120));
		// Unknown decoded rate / time: no verdict.
		assert!(!decoder_overloaded(12.0, 0.0, 120));
		assert!(!decoder_overloaded(0.0, 80.0, 120));
	}

	#[test]
	fn a_decoder_that_drops_or_falls_behind_sheds() {
		assert!(!decoder_sheds(120, 118, 0), "pipeline depth is noise");
		assert!(decoder_sheds(120, 90, 0), "a quarter not decoded");
		assert!(decoder_sheds(120, 119, 3), "the backlog guard dropped AUs");
		assert!(!decoder_sheds(4, 0, 0), "a static screen says nothing");
	}

	#[test]
	fn repair_window_follows_median_rtt_and_jitter() {
		// No samples: the upper bound.
		assert_eq!(repair_window_ms(&rtts(&[]), 0.0), REPAIR_MAX_MS);
		// A steady LAN: the floor.
		assert_eq!(repair_window_ms(&rtts(&[2.0, 2.0, 3.0]), 0.0), REPAIR_MIN_MS);
		// A steady 60 ms WAN: RTT + slack.
		let w = repair_window_ms(&rtts(&[60.0, 60.0, 60.0]), 0.0);
		assert!((w - 80.0).abs() < 0.01, "{w}");
		// Jitter widens it; FEC hold adds to it.
		assert!(repair_window_ms(&rtts(&[40.0, 80.0, 60.0, 100.0]), 0.0) > 80.0);
		assert!(repair_window_ms(&rtts(&[60.0]), 30.0) > repair_window_ms(&rtts(&[60.0]), 0.0));
		// A slow path never past the cap.
		assert_eq!(repair_window_ms(&rtts(&[400.0, 450.0]), 0.0), REPAIR_MAX_MS);
		assert!((rtt_jitter_ms(&rtts(&[4.0, 4.0, 5.0])) - 1.0 / 3.0).abs() < 0.01);
	}

	#[test]
	fn fast_drop_only_in_game_mode_on_a_high_rtt_path_with_keyframe_free_recovery() {
		assert!(fast_drop_on(false, true, true, 40.0));
		assert!(!fast_drop_on(false, false, true, 40.0), "remote desktop keeps the repair wait");
		assert!(!fast_drop_on(false, true, false, 40.0), "a keyframe recovery needs the repair");
		assert!(!fast_drop_on(false, true, true, 30.0), "under the on threshold");
		assert!(fast_drop_on(true, true, true, 30.0), "hysteresis keeps it on");
		assert!(!fast_drop_on(true, true, true, 20.0));
	}

	#[test]
	fn fast_drop_wait_covers_reordering_and_fec_but_never_an_rtt() {
		assert_eq!(fast_drop_wait_ms(3.0, 120), FAST_DROP_MIN_MS);
		let w = fast_drop_wait_ms(41.0, 60);
		assert!(w > 60.0 && w <= FAST_DROP_MAX_MS, "{w}");
		assert_eq!(fast_drop_wait_ms(200.0, 30), FAST_DROP_MAX_MS);
	}

	#[test]
	fn fec_wait_is_two_frames_bounded() {
		assert!((fec_wait_ms(60) - (2000.0 / 60.0 + 8.0)).abs() < 0.01);
		assert!((fec_wait_ms(240) - (2000.0 / 240.0 + 8.0)).abs() < 0.01);
		assert_eq!(fec_wait_ms(10), 50.0);
		assert_eq!(fec_wait_ms(0), fec_wait_ms(60));
		assert!(fec_active(Some(ms(200))));
		assert!(!fec_active(Some(Duration::from_secs(2))));
		assert!(!fec_active(None));
	}

	#[test]
	fn the_nack_waits_for_reordering_and_for_fec() {
		assert_eq!(nack_wait_ms(false, 60, 0.0), REORDER_WAIT_MIN_MS);
		assert_eq!(nack_wait_ms(false, 60, 100.0), REORDER_WAIT_MAX_MS);
		assert!((nack_wait_ms(true, 60, 0.0) - fec_wait_ms(60)).abs() < 0.01);
	}

	#[test]
	fn a_late_original_is_reordering_not_a_repair() {
		let t = Instant::now();
		assert!(!arrival_is_repair(None, t, 20.0));
		assert!(!arrival_is_repair(Some(t), t + ms(5), 20.0));
		assert!(arrival_is_repair(Some(t), t + ms(15), 20.0));
	}

	#[test]
	fn goodput_from_window_bytes() {
		assert_eq!(goodput_kbps(250_000, 2000), 1000);
		assert_eq!(goodput_kbps(1, 0), 0);
	}

	#[test]
	fn nack_waits_for_fec_until_it_gave_up() {
		assert!(!fec_block_gave_up(1, 2, 0));
		assert!(fec_block_gave_up(3, 2, 0), "more missing than parity");
		assert!(fec_block_gave_up(1, 2, 1), "the last shard is here");
		let t0 = Instant::now();
		let miss = |n: Option<Instant>| Miss { t0, nacked: n, renacked: false };
		let mut missing = HashMap::new();
		missing.insert(101u16, miss(None));
		missing.insert(103u16, miss(None));
		missing.insert(200u16, miss(Some(t0)));
		assert_eq!(missing_in(&missing, 100, 4), 2);
		let mut got = take_block_nacks(&mut missing, 100, 4, t0);
		got.sort();
		assert_eq!(got, vec![101, 103]);
		assert!(take_block_nacks(&mut missing, 100, 4, t0).is_empty(), "NACKed once");
		let mut pending: VecDeque<(u16, Instant)> = VecDeque::new();
		missing.insert(300u16, miss(None));
		missing.insert(301u16, miss(None));
		missing.remove(&300);
		pending.push_back((300, t0));
		pending.push_back((301, t0));
		assert!(take_due_fec_nacks(&mut pending, &mut missing, ms(30), t0 + ms(10)).is_empty());
		assert_eq!(take_due_fec_nacks(&mut pending, &mut missing, ms(30), t0 + ms(40)), vec![301]);
		assert!(pending.is_empty());
	}

	#[test]
	fn rebuilt_seqs_are_remembered_bounded() {
		let mut r = VecDeque::new();
		for s in 0..(FEC_REBUILT_KEEP as u16 + 10) {
			note_rebuilt(&mut r, s);
		}
		assert_eq!(r.len(), FEC_REBUILT_KEEP);
		assert_eq!(r.front(), Some(&10));
	}

	#[test]
	fn rtt_window_keeps_the_recent_samples() {
		let mut w = RttWindow::new();
		assert_eq!(w.median(7.0), 7.0);
		for r in [100.0, 10.0, 10.0, 12.0] {
			w.on_sample(r);
		}
		assert_eq!(w.median(0.0), 12.0);
		assert!(w.ema() > 10.0 && w.ema() < 100.0);
		assert_eq!(w.take_samples(), vec![100.0, 10.0, 10.0, 12.0]);
		assert!(w.take_samples().is_empty());
		for _ in 0..20 {
			w.on_sample(5.0);
		}
		assert_eq!(w.recent().len(), RTT_RECENT);
		assert_eq!(w.median(0.0), 5.0);
	}

	#[test]
	fn the_frame_clock_closes_frames_on_a_newer_timestamp_only() {
		let mut c = FrameClock::default();
		assert_eq!(c.on_packet(1000, 1.0), None);
		assert_eq!(c.on_packet(1000, 2.0), None);
		assert_eq!(c.on_packet(2500, 3.0), Some((1000, 2.0)));
		// A late retransmit of the old frame neither closes nor re-opens it.
		assert_eq!(c.on_packet(1000, 4.0), None);
		assert_eq!(c.on_packet(2500, 5.0), None);
		assert_eq!(c.on_packet(4000, 6.0), Some((2500, 5.0)));
		// Wrap-around is newer.
		let mut w = FrameClock::default();
		w.on_packet(u32::MAX - 10, 1.0);
		assert_eq!(w.on_packet(20, 2.0), Some((u32::MAX - 10, 1.0)));
		w.reset();
		assert_eq!(w.on_packet(5, 3.0), None);
		assert_eq!(rtp_timestamp(&rtp(1, 77, 4)), Some(77));
		assert_eq!(rtp_timestamp(&[0; 6]), None);
	}

	#[test]
	fn a_gap_waits_the_reorder_wait_then_is_nacked_once_and_renacked_once() {
		let t0 = Instant::now();
		let mut tr = RepairTracker::new();
		tr.on_rtt(20.0);
		assert!(tr.on_video(10, &rtp(10, 0, 100), t0, true, 60).nacks.is_empty());
		// 11 and 12 missing: held back for the reorder wait (3 ms min).
		let o = tr.on_video(13, &rtp(13, 0, 100), t0, true, 60);
		assert!(o.nacks.is_empty() && !o.restart);
		assert_eq!(tr.missing_count(), 2);
		// The next packet after the wait sends them.
		let mut n = tr.on_video(14, &rtp(14, 0, 100), t0 + ms(5), true, 60).nacks;
		n.sort();
		assert_eq!(n, vec![11, 12]);
		// No answer for ~1 RTT: re-NACKed once by the sweep, never twice.
		assert!(tr.sweep(t0 + ms(20), 60, false, true).nacks.is_empty());
		let mut r = tr.sweep(t0 + ms(50), 60, false, true).nacks;
		r.sort();
		assert_eq!(r, vec![11, 12]);
		assert!(tr.sweep(t0 + ms(55), 60, false, true).nacks.is_empty());
		// 11's retransmit lands: a repair. 12 is written off after the repair window (60 ms min).
		tr.on_video(11, &rtp(11, 0, 100), t0 + ms(56), true, 60);
		let s = tr.sweep(t0 + ms(70), 60, false, true);
		assert_eq!(s.expired, 1);
		assert_eq!(tr.missing_count(), 0);
		let w = tr.take_window();
		assert_eq!(w.recv, 4);
		assert_eq!(w.lost, 1, "12 never came");
		assert_eq!(w.nack_sent, 4, "2 NACKs + 2 re-NACKs");
		assert_eq!(w.nack_ok, 1);
		assert_eq!(w.recv_fresh, 4);
		assert_eq!(w.recv_bytes, 4 * 112);
		assert_eq!(w.rtt_ms, vec![20.0]);
		let s = w.sample(2000);
		assert_eq!((s.recv, s.lost, s.nack_sent, s.nack_ok, s.recv_bytes, s.window_ms), (4, 1, 4, 1, 448, 2000));
		// The window reset.
		assert_eq!(tr.take_window().recv, 0);
	}

	#[test]
	fn nacks_off_counts_but_sends_nothing() {
		let t0 = Instant::now();
		let mut tr = RepairTracker::new();
		tr.on_video(1, &rtp(1, 0, 10), t0, false, 60);
		tr.on_video(3, &rtp(3, 0, 10), t0, false, 60);
		assert!(tr.on_video(4, &rtp(4, 0, 10), t0 + ms(10), false, 60).nacks.is_empty());
		assert!(tr.sweep(t0 + ms(500), 60, false, false).nacks.is_empty());
		let w = tr.take_window();
		assert_eq!((w.lost, w.nack_sent), (1, 0));
	}

	#[test]
	fn a_late_original_feeds_the_reorder_wait_not_the_repairs() {
		let t0 = Instant::now();
		let mut tr = RepairTracker::new();
		tr.on_rtt(40.0);
		tr.on_video(1, &rtp(1, 0, 10), t0, true, 60);
		tr.on_video(3, &rtp(3, 0, 10), t0, true, 60);
		// 2 arrives 8 ms late, before its NACK: reordering.
		tr.on_video(2, &rtp(2, 0, 10), t0 + ms(2), true, 60);
		tr.on_video(4, &rtp(4, 0, 10), t0 + ms(8), true, 60);
		let w = tr.take_window();
		assert_eq!((w.lost, w.nack_ok, w.reordered, w.nack_sent), (0, 0, 1, 0));
		assert!(w.reorder_wait_ms >= REORDER_WAIT_MIN_MS);
	}

	#[test]
	fn duplicates_and_stale_reorders_are_not_fresh_and_a_new_base_resyncs() {
		let t0 = Instant::now();
		let mut tr = RepairTracker::new();
		tr.on_video(100, &rtp(100, 0, 10), t0, true, 60);
		tr.on_video(100, &rtp(100, 0, 10), t0, true, 60);
		tr.on_video(101, &rtp(101, 0, 10), t0, true, 60);
		tr.on_video(90, &rtp(90, 0, 10), t0, true, 60);
		let w = tr.take_window();
		assert_eq!((w.recv, w.recv_fresh, w.lost), (4, 2, 0));
		// A big forward jump and a big backward jump: an encoder restart, no loss counted.
		assert!(tr.on_video(5000, &rtp(5000, 0, 10), t0, true, 60).restart);
		assert!(tr.on_video(10, &rtp(10, 0, 10), t0, true, 60).restart);
		assert!(!tr.on_video(11, &rtp(11, 0, 10), t0, true, 60).restart);
		assert_eq!(tr.take_window().lost, 0);
	}

	#[test]
	fn renderer_wait_is_the_repair_window_plus_a_sweep() {
		let t0 = Instant::now();
		let mut tr = RepairTracker::new();
		assert_eq!(tr.renderer_wait_ms(t0, 60, false), RENDERER_WAIT_MAX_MS);
		for _ in 0..4 {
			tr.on_rtt(60.0);
		}
		assert!((tr.renderer_wait_ms(t0, 60, false) - (80.0 + REPAIR_SWEEP_MS as f32)).abs() < 0.01);
		assert_eq!(tr.renderer_wait_ms(t0, 120, true), FAST_DROP_MIN_MS);
	}

	#[test]
	fn the_fec_share_is_learned_per_window_and_released_when_parity_stops() {
		use super::super::controller::Config;
		use crate::pipeline::VCodec;
		let t0 = Instant::now();
		let mut ctl = Controller::new(Config::new(VCodec::H264, 20_000));
		let mut tr = RepairTracker::new();
		let w = Window { recv_bytes: 100_000, parity_bytes: 10_000, ..Default::default() };
		tr.last_parity_at = Some(t0);
		assert_eq!(tr.update_fec_share(&mut ctl, t0, &w), FecShare::Set(0.1));
		assert!((ctl.fec_overhead() - 0.1).abs() < 0.01);
		// Same share: kept.
		assert_eq!(tr.update_fec_share(&mut ctl, t0, &w), FecShare::Kept);
		// No parity for over a second: released.
		let none = Window { recv_bytes: 100_000, ..Default::default() };
		assert_eq!(tr.update_fec_share(&mut ctl, t0 + Duration::from_secs(2), &none), FecShare::Released);
		assert_eq!(ctl.fec_overhead(), 0.0);
	}

	#[test]
	fn rs_parity_rebuilds_a_missing_packet_with_no_round_trip() {
		let t0 = Instant::now();
		let mut enc = media::RsFecEncoder::new(0.25);
		// One frame (marker on the last packet) of 4 packets → 1 parity shard.
		let pkts: Vec<Vec<u8>> = (0..4u16)
			.map(|i| {
				let mut p = rtp(200 + i, 9000, 50 + i as usize);
				if i == 3 {
					p[1] |= 0x80;
				}
				p
			})
			.collect();
		let mut parity = Vec::new();
		for (i, p) in pkts.iter().enumerate() {
			parity.extend(enc.push(200 + i as u16, p));
		}
		parity.extend(enc.flush_pending());
		assert!(!parity.is_empty(), "the encoder emitted parity");
		let mut tr = RepairTracker::new();
		// 201 is lost.
		for (i, p) in pkts.iter().enumerate() {
			if i != 1 {
				tr.on_video(200 + i as u16, p, t0, true, 60);
			}
		}
		assert_eq!(tr.missing_count(), 1);
		let mut rebuilt = Vec::new();
		for par in &parity {
			let (tag, body) = media::parse(par).expect("a media frame");
			assert_eq!(tag, media::TAG_FEC_RS);
			rebuilt.extend(tr.on_rs_parity(body, t0, true).rebuilt);
		}
		assert_eq!(rebuilt.len(), 1);
		assert_eq!(rebuilt[0].0, 201);
		assert_eq!(rebuilt[0].1, pkts[1]);
		assert_eq!(tr.missing_count(), 0);
		let w = tr.take_window();
		assert_eq!((w.lost, w.fec_ok), (0, 1));
		assert!(w.parity_bytes > 0);
		// The late original of a rebuilt packet is reordering, not a repair.
		tr.on_video(201, &pkts[1], t0, true, 60);
		let w = tr.take_window();
		assert_eq!((w.fec_ok, w.reordered), (0, 1));
	}
}
