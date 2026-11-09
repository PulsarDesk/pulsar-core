//! Forward-error-correction sizing (adaptive streaming Phase 2.1 / 2.4): how many media
//! packets one XOR parity packet should cover, given the loss the client reports.
//!
//! Decision taken for the design doc's open question 2: parity overhead is capped at
//! **20 %** of the media rate, parity covers *every* packet (keyframes included — they are
//! the ones whose loss hurts most), and FEC switches off again after a clean stretch.
//! (That 20 % is the XOR groups'; the host's Reed-Solomon sizing is [`FecPolicy`] below —
//! pre-repair loss, proactive on WAN, up to [`RS_MAX_RATIO_HIGH`].)

/// Overhead ceiling (parity bytes / media bytes).
pub const MAX_OVERHEAD: f32 = 0.20;
/// Smallest group (= the 20 % ceiling) and the largest group worth sending.
pub const MIN_N: u8 = 5;
pub const MAX_N: u8 = 32;
/// Loss below which FEC is not worth its overhead.
pub const OFF_BELOW: f32 = 0.005;
/// Clean windows before FEC switches off again.
pub const OFF_AFTER_CLEAN: u32 = 5;

/// Group size for the next parity packets: `0` = FEC off. `loss` is the client's measured
/// ratio for the last window, `prev_n` the size currently in use, `clean_windows` how many
/// consecutive windows sat under `OFF_BELOW`. Parity is sized at roughly **twice** the loss
/// rate (one parity per group repairs one loss, so 2× keeps the repair probability high),
/// clamped to the overhead ceiling, with a ±25 % hysteresis so the size does not chatter.
pub fn group_size(loss: f32, prev_n: u8, clean_windows: u32) -> u8 {
	if loss < OFF_BELOW {
		return if prev_n == 0 || clean_windows >= OFF_AFTER_CLEAN { 0 } else { prev_n };
	}
	let overhead = (loss * 2.0).clamp(1.0 / MAX_N as f32, MAX_OVERHEAD);
	let want = ((1.0 / overhead).round() as u8).clamp(MIN_N, MAX_N);
	if prev_n > 0 {
		let band = (prev_n / 4).max(1);
		if want.abs_diff(prev_n) <= band {
			return prev_n;
		}
	}
	want
}

/// What share of the requested wire rate the *encoder* may use when parity of group size
/// `n` rides along (`n/(n+1)`); 1.0 when FEC is off.
pub fn encoder_share(n: u8) -> f32 {
	if n == 0 {
		1.0
	} else {
		n as f32 / (n as f32 + 1.0)
	}
}

/// Encoder share for a parity *overhead ratio* (parity bytes / media bytes): `1/(1+r)`.
pub fn encoder_share_ratio(overhead: f32) -> f32 {
	1.0 / (1.0 + overhead.max(0.0))
}

// ── Reed-Solomon parity (the gold-standard model: Sunshine/Moonlight, Parsec) ──────────
//
// Per video frame the host appends `m = ceil(k × ratio)` parity packets to the frame's `k`
// packets; the client rebuilds ANY `≤ m` lost packets of that frame with zero round-trips.
// Sunshine ships a fixed 20 % by default; here the ratio follows the measured loss so a
// clean path pays nothing and a bad one gets up to 30 %.

/// Lowest ratio once FEC is on (below this a single parity per frame is all it buys).
pub const RS_MIN_RATIO: f32 = 0.10;
/// Ceiling — the overhead the maintainer accepts on a very lossy path.
pub const RS_MAX_RATIO: f32 = 0.30;
/// Ratio changes smaller than this are ignored (no chatter).
pub const RS_HYSTERESIS: f32 = 0.05;

/// Parity ratio for the next frames: `0.0` = FEC off. Sized at ~2.5× the measured loss
/// (+5 % base) so a frame survives a burst, clamped to `[RS_MIN_RATIO, RS_MAX_RATIO]`; same
/// on/off hysteresis as [`group_size`].
pub fn parity_ratio(loss: f32, prev: f32, clean_windows: u32) -> f32 {
	if loss < OFF_BELOW {
		return if prev <= 0.0 || clean_windows >= OFF_AFTER_CLEAN { 0.0 } else { prev };
	}
	let want = (loss * 2.5 + 0.05).clamp(RS_MIN_RATIO, RS_MAX_RATIO);
	if prev > 0.0 && (want - prev).abs() < RS_HYSTERESIS {
		return prev;
	}
	want
}

// ── Host-side RS policy v2: sized from PRE-repair loss, proactive on WAN paths ───────────
//
// `parity_ratio` above follows the loss the client reports, which is measured AFTER the
// NACK / FEC repairs: once parity works, that loss falls to ~0, the ratio switches itself
// off, the loss comes back, and the parity returns a few windows later — a sawtooth that
// left every other window unprotected on a lossy mobile link. [`FecPolicy`] instead
// rebuilds the RAW loss (repaired + unrepaired) from the stats the client already sends,
// so working parity no longer argues for its own removal. On a path that crosses the
// internet it also starts with parity on (the first losses of a session are the ones that
// cost a keyframe) and keeps a floor while the link has been lossy recently; a clean LAN
// keeps the old behaviour (off until loss shows up). Wire format unchanged: the host just
// picks a different ratio for the same `TAG_FEC_RS` / `TAG_FEC` parity.
//
// Escalation and the wire budget (field test 2026-11-09: a ~9.6 Mbit/s packet policer, 1 %
// random loss, 78 ms RTT). The loss there was CAUSED by our own rate — the ABR stepping into
// the policer — and the policy answered it with 15 → 50 % parity in one window: ~3.5 Mbit/s
// more into the full bottleneck, plus NACK retransmits nobody budgeted. Video + parity +
// retransmits reached 12-16 Mbit/s against the client's ~9-10 Mbit/s target, so the loss fed
// itself (more loss → more parity → more loss). Two rules now:
//
// * Parity rises only for loss our rate did not cause. A window whose loss rose while the
//   video rate rose over the last window the parity covered, or that sits at a client rate cut
//   (this window or the previous one), is congestion: the ratio holds and the client's ABR
//   handles it — only a lower rate fixes a full queue. A raise needs the loss in two windows
//   running at a flat or falling video rate (random / non-congestive loss) and moves at most
//   `MAX_UP_STEP` per window, so the client's encoder deduction (one window behind) keeps up.
// * Wire budget: the next window's video + parity + retransmits stay within `BUDGET_MARGIN` of
//   the client's wire target (see [`budget_cap`]). The parity yields first — it is the part the
//   host chooses — but never below the WAN floor while loss is recent, nor below `RS_MIN_RATIO`
//   once on; whatever is still over comes out of the retransmits (the forwarder's pacer caps
//   their share of the wire).

/// Ceiling of the v2 policy on a very lossy path (RS(k, m) at 50 % repairs a third of a
/// block lost; beyond that only a lower rate helps, which is the controller's job).
pub const RS_MAX_RATIO_HIGH: f32 = 0.50;
/// Parity a WAN session starts with, and the floor it keeps while loss is recent.
pub const WAN_FLOOR: f32 = 0.15;
/// How long (ms) the WAN floor holds after the last lossy window (the session start counts
/// as one, so a clean WAN session pays the floor for its first 30 s only).
pub const WAN_HOLD_MS: u64 = 30_000;
/// A path with a round-trip at least this long is treated as WAN whatever its address says
/// (a ZeroTier / VPN overlay hands out private addresses across the internet).
pub const WAN_RTT_MS: f32 = 40.0;
/// ...and one with a round-trip below this is treated as LAN: NACK repairs within a frame or
/// two, so proactive parity buys nothing (also the relay → direct upgrade of a same-LAN pair,
/// which started on the relay and so looked WAN at the session start).
pub const LAN_RTT_MS: f32 = 10.0;
/// The client's stats window (`play/hold.rs` ticks every 2 s) — the span its NACK/FEC
/// counters cover, used to normalise the host's own per-window packet count.
pub const STATS_WINDOW_MS: u32 = 2000;
/// Per-window decay of the raw-loss estimate: it follows a rise at once but falls only by
/// this factor per window, so one lucky window doesn't drop the parity a burst needs.
const LOSS_DECAY: f32 = 0.7;
/// Consecutive windows that must ask for less parity before the ratio steps down.
const DOWN_AFTER: u32 = 2;
/// Smallest upward change applied (smaller ones are noise).
const UP_STEP: f32 = 0.02;
/// Rough RTP payload of a full video packet (the packetizer's MTU minus headers) — only the
/// fallback packet-count estimate uses it.
const EST_PKT_BYTES: f32 = 1200.0;
/// Most a loss-driven raise adds to the ratio in one window (15 → 50 % takes four windows of
/// persisting loss instead of one).
pub const MAX_UP_STEP: f32 = 0.10;
/// How far video + parity + retransmits may exceed the client's wire target before the parity
/// yields ([`budget_cap`]).
pub const BUDGET_MARGIN: f32 = 0.10;
/// The video rate counts as risen when it is this much above the reference window's: the ABR's
/// smallest climb is a +10 % probe, while a fixed target jitters ~1-2 % per window (field log).
const RISE_TOL: f32 = 0.05;
/// A wire target this much below the previous window's is the client's ABR cutting (its
/// smallest cut is ×0.85). Judged on the reconstructed wire target, not the encoder rate: the
/// encoder rate also drops when the client deducts a parity step of ours.
const CUT_TOL: f32 = 0.10;
/// Raw loss this far (absolute) above the previous window's counts as a loss rise.
const LOSS_RISE: f32 = 0.02;

/// Whether `ip` is a same-site address (RFC 1918 / ULA, link-local, loopback): a direct
/// session to one of these is a LAN, anything else crosses the internet.
pub fn is_lan_ip(ip: std::net::IpAddr) -> bool {
	let v4_lan = |v4: std::net::Ipv4Addr| v4.is_private() || v4.is_link_local() || v4.is_loopback();
	match ip {
		std::net::IpAddr::V4(v4) => v4_lan(v4),
		std::net::IpAddr::V6(v6) => {
			if let Some(v4) = v6.to_ipv4_mapped() {
				return v4_lan(v4);
			}
			let seg0 = v6.segments()[0];
			v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
		}
	}
}

/// The frame rate of an operating-point label (`"1080p120"` → 120, as `Point::label` writes
/// it); `0` for an empty / unknown label (the client sends none while the user pinned a pick).
pub fn point_label_fps(label: &str) -> u32 {
	label.rsplit_once('p').and_then(|(_, fps)| fps.parse().ok()).unwrap_or(0)
}

/// One client stats window as the host sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FecWindow {
	/// `ClientStats::loss` — unrepaired loss ratio after NACK + FEC.
	pub loss: f32,
	/// Packets the client got back by NACK retransmit / rebuilt from parity in the window.
	pub nack_ok: u32,
	pub fec_ok: u32,
	/// First-copy video packets the host sent since the previous stats (`0` = not counted;
	/// then the packet count is estimated from `kbps` / `fps`).
	pub sent_pkts: u64,
	/// Host time since the previous stats arrived (ms, `0` = unknown → one client window).
	pub window_ms: u32,
	/// The client's current target (kbit/s) and frame rate (`0` = unknown) — the fallback
	/// packet-count estimate. `kbps` is `ClientStats::kbps`: the ENCODER rate the client asks
	/// for, already net of the parity share its controller deducts (see [`budget_cap`]).
	pub kbps: u32,
	pub fps: u32,
	/// The client's latest round-trip (ms, `0` = no sample).
	pub rtt_ms: f32,
	/// What else the host put on the wire since the previous stats (session payload bytes,
	/// `0` = not counted): the first-copy video bytes (`sent_pkts` is their packet count), the
	/// parity packets / bytes and the NACK retransmit bytes. The congestion test and the wire
	/// budget read them; without them the test falls back to `kbps` and the budget is off.
	pub video_bytes: u64,
	pub parity_pkts: u64,
	pub parity_bytes: u64,
	pub rtx_bytes: u64,
}

/// Parity ratio the v2 policy sizes for a (raw) loss: ~2.5× the loss + 5 %, within
/// `[RS_MIN_RATIO, RS_MAX_RATIO_HIGH]`; `0.0` under `OFF_BELOW`.
fn sized(loss: f32) -> f32 {
	if loss < OFF_BELOW {
		0.0
	} else {
		(loss * 2.5 + 0.05).clamp(RS_MIN_RATIO, RS_MAX_RATIO_HIGH)
	}
}

/// The client's wire target (kbit/s) as the host reconstructs it, and the window's parity
/// share by packets (what the client deducts), or `None` without host counters / a client rate.
///
/// What the host knows: `kbps` is the client's ENCODER rate — its controller gives the encoder
/// `target / (1 + m/k)`, `m/k` = the parity overhead of the blocks it receives — so the target
/// it holds the wire to is `kbps × (1 + o)`, `o` = parity / video packets of the window (in the
/// 2026-11-09 field log this matched the client's own logged target within ~2 %).
fn wire_target(w: &FecWindow) -> Option<(f32, f32)> {
	if w.video_bytes == 0 || w.sent_pkts == 0 || w.kbps == 0 {
		return None;
	}
	let o = w.parity_pkts as f32 / w.sent_pkts as f32;
	Some((w.kbps as f32 * (1.0 + o), o))
}

/// The wire budget: the highest ratio the next window may use, given the ratio `prev` the
/// window `w` was sent with (`None` = no host counters → no budget).
///
/// Rule: next window's video + parity + retransmits ≤ the client's wire target `T` (see
/// `wire_target`) × (1 + `BUDGET_MARGIN`). Next window the encoder sends about `kbps` (what
/// the client just asked for); parity at ratio `r` costs `kbps × (o_bytes + r − prev)` (the
/// per-block rounding up of `m` is a fixed add-on to the ratio, so the overhead moves with `r`
/// one for one); retransmits about what they cost this window — they follow the loss, which
/// the parity change does not undo at once. Solved for `r`:
/// `r ≤ prev + margin × (1 + o) − (o_bytes − o) − rtx / kbps` — the parity may grow by what the
/// margin leaves after the retransmits, and must shrink once they take more than the margin.
pub fn budget_cap(w: &FecWindow, prev: f32) -> Option<f32> {
	let (_, o) = wire_target(w)?;
	let kbps = w.kbps as f32;
	let win = (if w.window_ms == 0 { STATS_WINDOW_MS } else { w.window_ms }) as f32;
	let rtx_kbps = w.rtx_bytes as f32 * 8.0 / win;
	// Parity shards are as long as the longest packet of their block, so by bytes the overhead
	// sits a little above the packet share the client deducts; that excess is wire too.
	let o_bytes = w.parity_bytes as f32 / w.video_bytes as f32;
	Some(prev + BUDGET_MARGIN * (1.0 + o) - (o_bytes - o).max(0.0) - rtx_kbps / kbps)
}

/// Pre-repair loss ratio of a window: the unrepaired loss plus every packet NACK or parity
/// got back, over the packets the window carried. `sent_pkts` (host count) is preferred; it is
/// scaled to the client's 2 s window when stats arrived late (a skipped window would
/// otherwise halve the estimate). Without it the count is estimated from the rate and the
/// frame rate (each frame is at least one packet), which errs high on raw loss — the safe side.
pub fn raw_loss(w: &FecWindow) -> f32 {
	let post = w.loss.clamp(0.0, 1.0);
	let repaired = w.nack_ok as f32 + w.fec_ok as f32;
	if repaired <= 0.0 {
		return post;
	}
	let pkts = if w.sent_pkts > 0 {
		let win = (if w.window_ms == 0 { STATS_WINDOW_MS } else { w.window_ms }) as f32;
		let full = STATS_WINDOW_MS as f32;
		let scale = if win > full * 1.25 { full / win } else { 1.0 };
		w.sent_pkts as f32 * scale
	} else {
		let secs = STATS_WINDOW_MS as f32 / 1000.0;
		let by_bytes = w.kbps as f32 * 1000.0 / 8.0 * secs / EST_PKT_BYTES;
		let by_frames = w.fps as f32 * secs;
		by_bytes.max(by_frames)
	};
	if pkts < 1.0 {
		return post;
	}
	(post + repaired / pkts).clamp(post, 1.0)
}

/// The host's FEC sizing state for one session (v2, see the section comment). Feed it every
/// client stats window ([`on_window`](Self::on_window)); it returns the RS parity ratio to
/// use (`0.0` = off), which the forwarder also maps to XOR groups for XOR-only clients.
#[derive(Clone, Debug)]
pub struct FecPolicy {
	wan_path: bool,
	ratio: f32,
	/// Fast-attack / slow-decay estimate of the raw loss.
	loss_est: f32,
	last_raw: f32,
	/// Ms since the last lossy window (the session start counts as lossy on WAN).
	since_loss_ms: u64,
	clean_windows: u32,
	down_votes: u32,
	/// Video rate (kbit/s) of the last window whose loss the parity in use covered: the rate the
	/// path carried acceptably, which a loss rise is judged against (0 = none yet).
	covered_kbps: f32,
	/// The previous window's video rate and reconstructed client wire target (kbit/s, 0 = none).
	prev_kbps: f32,
	prev_target: f32,
	/// Whether the previous window's loss asked for parity above the floor / showed a client cut.
	lossy_last: bool,
	cut_last: bool,
	/// Consecutive windows with an internet-sized RTT (a LAN path is promoted to WAN on the
	/// second one).
	wan_rtt_votes: u8,
	/// Why the last window kept the ratio under what its loss asked for ("" = it did not).
	hold: &'static str,
	/// The last window's reconstructed client wire target (kbit/s, 0 = unknown).
	target_kbps: f32,
}

impl FecPolicy {
	/// `wan_path`: the session runs over the relay or directly to a non-LAN address.
	pub fn new(wan_path: bool) -> Self {
		Self {
			wan_path,
			ratio: if wan_path { WAN_FLOOR } else { 0.0 },
			loss_est: 0.0,
			last_raw: 0.0,
			since_loss_ms: if wan_path { 0 } else { u64::MAX / 2 },
			clean_windows: 0,
			down_votes: 0,
			covered_kbps: 0.0,
			prev_kbps: 0.0,
			prev_target: 0.0,
			lossy_last: false,
			cut_last: false,
			wan_rtt_votes: 0,
			hold: "",
			target_kbps: 0.0,
		}
	}

	/// The ratio in use (the start ratio before the first window).
	pub fn ratio(&self) -> f32 {
		self.ratio
	}

	/// The raw (pre-repair) loss of the last window.
	pub fn last_raw_loss(&self) -> f32 {
		self.last_raw
	}

	/// Whether the policy treats the path as WAN (relay / public address, or a long RTT seen).
	pub fn is_wan(&self) -> bool {
		self.wan_path
	}

	/// Why the last window kept the ratio under what its loss asked for: `"congestive"` (the loss
	/// came with a video-rate rise or a client cut), `"single-window"` (not persisting yet),
	/// `"rate-up"` (persisting, but the video rate is climbing), `"step-limit"` (raised by
	/// `MAX_UP_STEP` only), `"budget"` (the wire budget); `""` = no hold.
	pub fn hold_reason(&self) -> &'static str {
		self.hold
	}

	/// The client's wire target as reconstructed from the last window (kbit/s, 0 = unknown).
	pub fn target_kbps(&self) -> f32 {
		self.target_kbps
	}

	/// One client stats window → the parity ratio for the next frames.
	pub fn on_window(&mut self, w: &FecWindow) -> f32 {
		let raw = raw_loss(w);
		let prev_raw = self.last_raw;
		self.last_raw = raw;
		// Two windows in a row, not one: the session's first RTT samples are junk (pings queued
		// behind the stream setup — 244 ms ± 249 on a 4 ms LAN, 2026-11-09), and one of them
		// turned a clean LAN's proactive WAN parity on.
		self.wan_rtt_votes = if w.rtt_ms >= WAN_RTT_MS { self.wan_rtt_votes.saturating_add(1) } else { 0 };
		if self.wan_rtt_votes >= 2 && !self.wan_path {
			// An overlay path with an internet-sized RTT is WAN; its floor hold starts now.
			self.wan_path = true;
			self.since_loss_ms = 0;
		} else if w.rtt_ms > 0.0 && w.rtt_ms < LAN_RTT_MS && self.wan_path {
			// A LAN-sized RTT: NACK is fast enough. Parity that was only the WAN guess goes
			// at once; parity the loss asked for follows the LAN rules from here.
			self.wan_path = false;
			if self.loss_est < OFF_BELOW && raw < OFF_BELOW {
				self.clean_windows = self.clean_windows.max(OFF_AFTER_CLEAN);
			}
		}
		let win = (if w.window_ms == 0 { STATS_WINDOW_MS } else { w.window_ms }) as u64;
		if raw >= OFF_BELOW {
			self.since_loss_ms = 0;
			self.clean_windows = 0;
		} else {
			self.since_loss_ms = self.since_loss_ms.saturating_add(win);
			self.clean_windows = self.clean_windows.saturating_add(1);
		}
		self.loss_est = if raw >= self.loss_est { raw } else { (self.loss_est * LOSS_DECAY).max(raw) };
		// The proactive WAN floor: not loss-driven sizing, so none of the escalation rules below
		// apply to it (it comes back at once with the first loss after a clean stretch).
		let floor = if self.wan_path && self.since_loss_ms < WAN_HOLD_MS { WAN_FLOOR } else { 0.0 };
		let want = sized(self.loss_est).max(floor);
		let prev = self.ratio;

		// ── Is this loss ours? (see the section comment) ──
		let target = wire_target(w);
		// The video rate that went out in the window — the part the client's ABR drives. Our
		// own parity steps are left out on purpose: the budget bounds them, and counting them
		// here would read every step of ours as a rate rise in the window right after it.
		let video_kbps = if target.is_some() { w.video_bytes as f32 * 8.0 / win as f32 } else { w.kbps as f32 };
		let target_kbps = target.map_or(0.0, |(t, _)| t);
		let cut = self.prev_target > 0.0 && target_kbps > 0.0 && target_kbps < self.prev_target * (1.0 - CUT_TOL);
		let need = sized(raw);
		// A window whose own loss asks for parity above the floor (for persistence)...
		let lossy = need >= floor + UP_STEP;
		// ...and one whose loss the ratio in use covered (its rate is the reference a later
		// loss rise is judged against).
		let covered = need < prev + UP_STEP;
		let loss_rose = raw > prev_raw + LOSS_RISE;
		let rose_over_covered = self.covered_kbps > 0.0 && video_kbps > self.covered_kbps * (1.0 + RISE_TOL);
		let flat = self.prev_kbps <= 0.0 || video_kbps <= self.prev_kbps * (1.0 + RISE_TOL);
		let congestive = (loss_rose && rose_over_covered) || cut || self.cut_last;
		let persists = lossy && self.lossy_last;
		self.prev_kbps = video_kbps;
		self.prev_target = target_kbps;
		self.target_kbps = target_kbps;
		self.cut_last = cut;
		self.lossy_last = lossy;
		if covered {
			self.covered_kbps = video_kbps;
		}

		let mut hold = "";
		let asks_down = want > 0.0 && prev - want >= RS_HYSTERESIS;
		let mut next = if want <= 0.0 {
			// LAN: off only after a clean stretch (as before). WAN: the floor already held for
			// `WAN_HOLD_MS` of clean windows, so off now.
			if prev <= 0.0 || self.wan_path || self.clean_windows >= OFF_AFTER_CLEAN {
				0.0
			} else {
				prev
			}
		} else if prev <= 0.0 || want >= prev + UP_STEP {
			// Up. Holding keeps the ratio in use (and the floor); a loss-driven raise needs loss
			// we did not cause, persisting at a flat or falling rate, and is step-limited.
			let held = prev.max(floor);
			if want <= held {
				held
			} else if congestive {
				hold = "congestive";
				held
			} else if !persists {
				hold = "single-window";
				held
			} else if !flat {
				hold = "rate-up";
				held
			} else if want > held + MAX_UP_STEP {
				hold = "step-limit";
				held + MAX_UP_STEP
			} else {
				want
			}
		} else if asks_down {
			// Down only when asked twice in a row: an alternating good/bad link holds its level.
			self.down_votes += 1;
			if self.down_votes >= DOWN_AFTER {
				want
			} else {
				prev
			}
		} else {
			prev
		};
		// The wire budget overrides everything above, downward too and at once (no down votes):
		// the parity yields to what the retransmits already take. It never takes the minimum
		// protection away — the WAN floor while loss is recent, `RS_MIN_RATIO` whenever parity
		// is on — only what is sized above it: turning parity on costs about the whole margin,
		// so a budget that could veto it would leave a lossy LAN on NACK alone for good, and
		// whether parity runs at all stays the classifier's and the clean-stretch rule's call.
		if let Some(cap) = budget_cap(w, prev) {
			let keep = floor.max(if next > 0.0 { RS_MIN_RATIO } else { 0.0 });
			let cap = cap.max(keep);
			if next > cap + 1e-6 {
				next = cap;
				hold = "budget";
			}
		}
		if !asks_down || next != prev {
			self.down_votes = 0;
		}
		self.hold = hold;
		self.ratio = next;
		next
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sizes_follow_loss_within_the_overhead_ceiling() {
		assert_eq!(group_size(0.03, 0, 0), 17, "3 % loss → ~6 % parity");
		assert_eq!(group_size(0.10, 0, 0), 5, "10 % loss hits the 20 % ceiling");
		assert_eq!(group_size(0.30, 0, 0), 5, "never more than 20 % overhead");
		assert_eq!(group_size(0.01, 0, 0), 32, "1 % loss → the largest group");
		assert_eq!(group_size(0.004, 0, 0), 0, "below the floor: off");
	}

	#[test]
	fn hysteresis_and_switch_off_after_a_clean_stretch() {
		assert_eq!(group_size(0.035, 17, 0), 17, "small change keeps the size");
		assert_eq!(group_size(0.08, 17, 0), 6, "big change moves");
		assert_eq!(group_size(0.0, 17, 2), 17, "still on while the clean stretch is short");
		assert_eq!(group_size(0.0, 17, OFF_AFTER_CLEAN), 0, "off after the clean stretch");
	}

	#[test]
	fn rs_parity_ratio_follows_loss_within_bounds_with_hysteresis() {
		assert_eq!(parity_ratio(0.004, 0.0, 0), 0.0, "clean: off");
		assert!((parity_ratio(0.03, 0.0, 0) - 0.125).abs() < 1e-6, "3 % → 12.5 %");
		assert_eq!(parity_ratio(0.10, 0.0, 0), RS_MAX_RATIO, "10 % → ceiling");
		assert_eq!(parity_ratio(0.01, 0.0, 0), RS_MIN_RATIO, "1 % → floor");
		assert!((parity_ratio(0.04, 0.125, 0) - 0.125).abs() < 1e-6, "small change keeps");
		assert!(parity_ratio(0.08, 0.125, 0) > 0.2, "big change moves");
		assert!((parity_ratio(0.0, 0.125, 2) - 0.125).abs() < 1e-6, "short clean stretch keeps");
		assert_eq!(parity_ratio(0.0, 0.125, OFF_AFTER_CLEAN), 0.0, "off after the clean stretch");
		assert!((encoder_share_ratio(0.25) - 0.8).abs() < 1e-6);
		assert_eq!(encoder_share_ratio(0.0), 1.0);
	}

	#[test]
	fn encoder_share_leaves_room_for_parity() {
		assert_eq!(encoder_share(0), 1.0);
		assert!((encoder_share(5) - 0.8333).abs() < 0.001);
		assert!((encoder_share(32) - 0.9697).abs() < 0.001);
	}

	/// A 2 s window with `sent` host packets, `lost` unrepaired and `fec_ok` rebuilt.
	fn win(sent: u64, lost: u32, fec_ok: u32, rtt_ms: f32) -> FecWindow {
		let recv = sent as f32 - lost as f32 - fec_ok as f32;
		FecWindow {
			loss: lost as f32 / (recv + lost as f32).max(1.0),
			fec_ok,
			sent_pkts: sent,
			window_ms: 2000,
			kbps: 10_000,
			fps: 60,
			rtt_ms,
			..Default::default()
		}
	}

	#[test]
	fn raw_loss_adds_the_repaired_packets_back() {
		let w = win(1000, 10, 50, 5.0);
		assert!((raw_loss(&w) - (10.0 / 950.0 + 0.05)).abs() < 1e-4, "{}", raw_loss(&w));
		// NACK-repaired packets count as raw loss too.
		let w2 = FecWindow { nack_ok: 30, ..win(1000, 0, 0, 5.0) };
		assert!((raw_loss(&w2) - 0.03).abs() < 1e-6);
		// Nothing repaired → the post-repair loss as is (no packet count needed).
		assert_eq!(raw_loss(&FecWindow { loss: 0.2, ..Default::default() }), 0.2);
		// Stats arrived after 4 s (the client skipped a window): the host count is scaled to
		// the client's 2 s window, not halving the estimate.
		let late = FecWindow { window_ms: 4000, sent_pkts: 2000, fec_ok: 50, ..Default::default() };
		assert!((raw_loss(&late) - 0.05).abs() < 1e-6, "{}", raw_loss(&late));
		// No host count: estimated from the rate / frame rate (at least one packet per frame).
		let est = FecWindow { fec_ok: 24, kbps: 300, fps: 120, ..Default::default() };
		assert!((raw_loss(&est) - 0.1).abs() < 1e-6, "240 frames per window → {}", raw_loss(&est));
		// Never above 1.
		assert_eq!(raw_loss(&FecWindow { loss: 0.5, fec_ok: 5000, sent_pkts: 100, ..Default::default() }), 1.0);
	}

	#[test]
	fn clean_lan_stays_off() {
		let mut p = FecPolicy::new(false);
		assert_eq!(p.ratio(), 0.0, "LAN starts without parity");
		for _ in 0..30 {
			assert_eq!(p.on_window(&win(20_000, 0, 0, 1.0)), 0.0);
		}
		assert!(!p.is_wan());
	}

	#[test]
	fn lan_loss_turns_parity_on_then_off_after_a_clean_stretch() {
		let mut p = FecPolicy::new(false);
		// One lossy window is not persisting loss (it turned parity on at once before the
		// congestion fix); the second turns it on at the smallest ratio, the third sizes it.
		assert_eq!(p.on_window(&win(10_000, 300, 0, 2.0)), 0.0);
		assert_eq!(p.hold_reason(), "single-window");
		assert!((p.on_window(&win(10_000, 300, 0, 2.0)) - RS_MIN_RATIO).abs() < 1e-6);
		let r = p.on_window(&win(10_000, 300, 0, 2.0));
		assert!((r - 0.125).abs() < 1e-3, "3 % → 12.5 %, got {r}");
		let mut off_at = None;
		for i in 1..=20 {
			if p.on_window(&win(10_000, 0, 0, 2.0)) == 0.0 {
				off_at = Some(i);
				break;
			}
		}
		let off_at = off_at.expect("switches off on a clean LAN");
		assert!(off_at >= OFF_AFTER_CLEAN as usize, "not before the clean stretch ({off_at})");
	}

	/// The v1 bug: parity that repairs everything drives the post-repair loss to 0, which
	/// switched parity off, which brought the loss back. Sized from raw loss it holds steady.
	#[test]
	fn raw_loss_sizing_does_not_oscillate_when_parity_works() {
		let mut p = FecPolicy::new(false);
		// 8 % raw loss, first window unprotected.
		let mut r = p.on_window(&win(5000, 400, 0, 10.0));
		let mut ratios = Vec::new();
		for _ in 0..30 {
			// Parity now rebuilds every loss: post-repair loss 0, fec_ok carries the raw loss.
			let lost = if r > 0.0 { 0 } else { 400 };
			let fec_ok = if r > 0.0 { 400 } else { 0 };
			r = p.on_window(&win(5000, lost, fec_ok, 10.0));
			ratios.push(r);
		}
		// Since the congestion fix the size (2.5 × 8 % + 5 % = 25 %) is reached over a few
		// windows (persistence, ≤ `MAX_UP_STEP` a window) instead of at once — then it holds.
		assert!((ratios[0] - 0.10).abs() < 1e-6 && (ratios[1] - 0.20).abs() < 1e-6, "{ratios:?}");
		assert!(ratios[2..].iter().all(|&x| (x - 0.25).abs() < 1e-6), "steady: {ratios:?}");
		// The v1 policy on the same feedback falls off within a few windows.
		let (mut v1, mut clean) = (parity_ratio(0.08, 0.0, 0), 0u32);
		let mut v1_off = false;
		for _ in 0..30 {
			let loss = if v1 > 0.0 { 0.0 } else { 0.08 };
			clean = if loss < OFF_BELOW { clean + 1 } else { 0 };
			v1 = parity_ratio(loss, v1, clean);
			v1_off |= v1 == 0.0;
		}
		assert!(v1_off, "v1 switched itself off");
	}

	#[test]
	fn alternating_loss_does_not_flap_the_ratio_down() {
		let mut p = FecPolicy::new(false);
		p.on_window(&win(5000, 500, 0, 10.0));
		// The climb to the bad windows' size (10 % → 30 %) takes a few windows since the
		// congestion fix (it was there after the first); once there, the level holds.
		let mut seen = Vec::new();
		for i in 0..12 {
			let w = if i % 2 == 0 { win(5000, 0, 100, 10.0) } else { win(5000, 0, 500, 10.0) };
			seen.push(p.on_window(&w));
		}
		assert!(seen.windows(2).all(|s| s[1] >= s[0] - 1e-6), "never down: {seen:?}");
		assert!(seen[4..].iter().all(|&x| (x - 0.30).abs() < 1e-6), "holds 30 %: {seen:?}");
	}

	#[test]
	fn high_raw_loss_scales_to_the_50_percent_cap() {
		// Persisting heavy loss still reaches the cap — in `MAX_UP_STEP`s from the WAN floor
		// (since the congestion fix; it went 15 → 50 % in one window before).
		let mut p = FecPolicy::new(true);
		let seen: Vec<f32> = (0..6).map(|_| p.on_window(&win(2000, 200, 600, 80.0))).collect();
		for (got, want) in seen.iter().zip([WAN_FLOOR, 0.25, 0.35, 0.45, RS_MAX_RATIO_HIGH, RS_MAX_RATIO_HIGH]) {
			assert!((got - want).abs() < 1e-6, "40 % raw: {seen:?}");
		}
		assert!(seen.windows(2).all(|s| s[1] - s[0] <= MAX_UP_STEP + 1e-6), "rate-limited: {seen:?}");
		let mut q = FecPolicy::new(false);
		let seen: Vec<f32> = (0..4).map(|_| q.on_window(&win(2000, 0, 200, 3.0))).collect();
		assert!((seen[3] - 0.30).abs() < 1e-3, "10 % raw → 30 %: {seen:?}");
		q.on_window(&win(2000, 0, 400, 3.0));
		let r = q.on_window(&win(2000, 0, 400, 3.0));
		assert!((r - 0.50).abs() < 1e-3, "20 % raw → the cap, got {r}");
	}

	/// A 2 s window with the host's wire counters: the client asks its encoder for `kbps`, the
	/// video that went out ran at `video_kbps`, parity packets at `share` of the video packets,
	/// `rtx_kbps` of retransmits, and `raw` pre-repair loss (rebuilt by parity) at an 80 ms RTT.
	fn wire(kbps: u32, video_kbps: u32, share: f32, rtx_kbps: u32, raw: f32) -> FecWindow {
		let bytes = |k: u32| k as u64 * 1000 / 8 * 2;
		let video_bytes = bytes(video_kbps);
		let pkts = (video_bytes / 1150).max(1);
		let parity_pkts = (pkts as f32 * share).round() as u64;
		FecWindow {
			fec_ok: (pkts as f32 * raw).round() as u32,
			sent_pkts: pkts,
			window_ms: 2000,
			kbps,
			fps: 90,
			rtt_ms: 80.0,
			video_bytes,
			parity_pkts,
			parity_bytes: parity_pkts * 1200,
			rtx_bytes: bytes(rtx_kbps),
			..Default::default()
		}
	}

	/// The 2026-11-09 policer test (rounded from the host log): the ABR's climb lands on a
	/// ~9.6 Mbit/s policer and the loss is ours. The old policy answered with 15 → 50 % parity
	/// in that window; the parity now holds and the client's cut handles it.
	#[test]
	fn congestive_loss_holds_the_ratio() {
		let mut p = FecPolicy::new(true);
		// Startup, clean: the client climbs, the video follows.
		assert_eq!(p.on_window(&wire(7000, 3700, 0.25, 0, 0.017)), WAN_FLOOR);
		assert_eq!(p.on_window(&wire(10_800, 6300, 0.23, 0, 0.011)), WAN_FLOOR);
		// The step lands on the policer: video 6.3 → 10.7 Mbit, 31 % of it lost; the client
		// cuts its target 13.3 → 8.7 Mbit in the same window.
		assert_eq!(p.on_window(&wire(7533, 10_700, 0.155, 0, 0.31)), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "congestive");
		// The window after the cut still belongs to it.
		assert_eq!(p.on_window(&wire(7533, 7500, 0.155, 0, 0.24)), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "congestive");

		// A probe into the policer without a cut in that window is ours too: +17 % video and
		// the loss rises with it.
		let mut q = FecPolicy::new(true);
		for _ in 0..3 {
			assert_eq!(q.on_window(&wire(6954, 7000, 0.23, 0, 0.01)), WAN_FLOOR);
		}
		assert_eq!(q.on_window(&wire(8522, 8200, 0.23, 0, 0.12)), WAN_FLOOR);
		assert_eq!(q.hold_reason(), "congestive");
		// A slow climb past the covered rate counts as well (each window under the tolerance).
		let mut s = FecPolicy::new(true);
		s.on_window(&wire(7000, 7000, 0.23, 0, 0.01));
		s.on_window(&wire(7300, 7300, 0.23, 0, 0.06));
		assert_eq!(s.on_window(&wire(7600, 7600, 0.23, 0, 0.12)), WAN_FLOOR);
		assert_eq!(s.hold_reason(), "congestive");
	}

	#[test]
	fn random_loss_escalates_after_two_windows_at_a_flat_rate() {
		let mut p = FecPolicy::new(true);
		// 10 % random loss at a steady 7 Mbit (parity rebuilds it, nothing is retransmitted).
		let w = wire(7000, 7000, 0.23, 0, 0.10);
		assert_eq!(p.on_window(&w), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "single-window");
		assert!((p.on_window(&w) - (WAN_FLOOR + MAX_UP_STEP)).abs() < 1e-6);
		assert_eq!(p.hold_reason(), "step-limit");
		assert!((p.on_window(&w) - 0.30).abs() < 1e-3, "the loss's size: 2.5 × 10 % + 5 %");
		assert_eq!(p.hold_reason(), "");
		assert!((p.on_window(&w) - 0.30).abs() < 1e-3, "holds");
		// A lone burst on an otherwise ~1 % WAN path is not persisting loss.
		let mut q = FecPolicy::new(true);
		for _ in 0..3 {
			q.on_window(&wire(7000, 7000, 0.23, 0, 0.01));
		}
		assert_eq!(q.on_window(&wire(7000, 7000, 0.23, 0, 0.30)), WAN_FLOOR);
		assert_eq!(q.hold_reason(), "single-window");
	}

	#[test]
	fn a_rising_rate_or_a_client_cut_holds_persisting_loss() {
		let mut p = FecPolicy::new(true);
		p.on_window(&wire(7000, 7000, 0.23, 0, 0.10));
		// The ABR climbs +20 % a window through the loss: not a flat rate, so no raise.
		assert_eq!(p.on_window(&wire(8400, 8400, 0.23, 0, 0.10)), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "rate-up");
		assert_eq!(p.on_window(&wire(10_000, 10_000, 0.23, 0, 0.10)), WAN_FLOOR);
		// The client cuts (target 12.3 → 8 Mbit): that window and the next hold...
		assert_eq!(p.on_window(&wire(6500, 9800, 0.23, 0, 0.10)), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "congestive");
		assert_eq!(p.on_window(&wire(6500, 6500, 0.23, 0, 0.10)), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "congestive");
		// ...and loss that persists at the lower, flat rate is not ours: the raise starts.
		assert!((p.on_window(&wire(6500, 6500, 0.23, 0, 0.10)) - (WAN_FLOOR + MAX_UP_STEP)).abs() < 1e-6);
		// The encoder rate falling only because the client deducts our step is no cut: its
		// wire target (encoder rate × (1 + parity share)) is unchanged.
		assert!(p.on_window(&wire(5980, 6000, 0.33, 0, 0.10)) > WAN_FLOOR + MAX_UP_STEP);
	}

	#[test]
	fn the_wire_budget_yields_parity_to_retransmits() {
		// Random 10 % at a flat 7 Mbit, but retransmits at 15 % of the encoder rate already take
		// the whole margin: the parity stays at the WAN floor.
		let mut p = FecPolicy::new(true);
		let w = wire(7000, 7000, 0.23, 1050, 0.10);
		p.on_window(&w);
		assert_eq!(p.on_window(&w), WAN_FLOOR);
		assert_eq!(p.hold_reason(), "budget");
		// Retransmits at 5 %: the margin left buys a smaller step than `MAX_UP_STEP`, and the
		// next window's wire stays within the margin over the client's target.
		let mut q = FecPolicy::new(true);
		let w = wire(7000, 7000, 0.23, 350, 0.10);
		q.on_window(&w);
		let prev = q.ratio();
		let cap = budget_cap(&w, prev).unwrap();
		let r = q.on_window(&w);
		assert!((r - cap).abs() < 1e-6 && r > WAN_FLOOR && r < WAN_FLOOR + MAX_UP_STEP, "{r} (cap {cap})");
		assert_eq!(q.hold_reason(), "budget");
		let (target, _) = wire_target(&w).unwrap();
		let o_bytes = w.parity_bytes as f32 / w.video_bytes as f32;
		let next_wire = 7000.0 * (1.0 + o_bytes + r - prev) + 350.0;
		assert!(next_wire <= target * (1.0 + BUDGET_MARGIN) + 1.0, "{next_wire} vs {target}");
		// Parity raised for random loss (45 %), then retransmits jump to 40 % of the rate: it
		// drops at once (no down votes) to what the budget leaves...
		let mut s = FecPolicy::new(true);
		for _ in 0..4 {
			s.on_window(&wire(7000, 7000, 0.23, 0, 0.40));
		}
		assert!((s.ratio() - 0.45).abs() < 1e-6, "{}", s.ratio());
		let burst = wire(7000, 7000, 0.45, 2800, 0.40);
		let cap = budget_cap(&burst, s.ratio()).unwrap();
		let r = s.on_window(&burst);
		assert!((r - cap).abs() < 1e-6 && r < 0.45 - RS_HYSTERESIS, "{r} (cap {cap})");
		// ...but never under the WAN floor while the loss is recent.
		assert_eq!(s.on_window(&wire(7000, 7000, 0.45, 7000, 0.40)), WAN_FLOOR);
		// LAN: the budget limits what is sized above the smallest ratio, it does not veto
		// turning parity on (NACK alone would carry a lossy LAN for good).
		// 8 % loss, all of it NACK-repaired: retransmits at 8 % of a 20 Mbit rate.
		let mut l = FecPolicy::new(false);
		let lan = |share| FecWindow { nack_ok: 348, fec_ok: 0, rtt_ms: 3.0, ..wire(20_000, 20_000, share, 1600, 0.0) };
		assert_eq!(l.on_window(&lan(0.0)), 0.0);
		assert_eq!(l.on_window(&lan(0.0)), RS_MIN_RATIO, "on, though the retransmits fill the margin");
		// Above it the budget applies: 10 % + margin × 1.1 − 8 % retransmits ≈ 13 %, not 20 %.
		let r = l.on_window(&lan(0.10));
		assert!(r > RS_MIN_RATIO && r < RS_MIN_RATIO + MAX_UP_STEP - 0.05, "{r}");
		assert_eq!(l.hold_reason(), "budget");
		// No host counters: no budget.
		assert!(budget_cap(&win(10_000, 0, 500, 80.0), 0.15).is_none());
	}

	/// The host's "client stats" windows of the 2026-11-09 policer run (~1040 pkt/s ≈ 9.6
	/// Mbit/s, 60-packet burst, 1 % random loss, 78 ms RTT) replayed through the policy. The
	/// traffic is what the OLD policy produced (its ratio in the `old ‰` column: 50 % in every
	/// congestion episode), so this checks the decisions, not a new outcome: the policy now
	/// stays near the WAN floor through all three episodes.
	#[test]
	fn field_policer_run_stays_near_the_floor() {
		// (time, loss, nack_ok, fec_ok, video_pkts, window_ms, kbps, video_kbps, parity_pkts,
		//  parity_kbps, rtx_kbps, old ‰, rtt)
		#[rustfmt::skip]
		let rows: [(&str, f32, u32, u32, u64, u32, u32, u64, u64, u64, u64, u32, f32); 28] = [
			("15:57:02", 0.0000, 0, 15, 2360, 3393, 10800, 6339, 552, 1574, 0, 150, 82.0),
			("15:57:04", 0.1652, 331, 9, 2334, 1999, 7533, 10697, 361, 1748, 2907, 500, 84.0),
			("15:57:06", 0.0722, 199, 19, 1320, 2002, 5683, 5669, 700, 3362, 1258, 500, 83.0),
			("15:57:08", 0.0490, 164, 12, 1474, 1997, 5795, 5767, 766, 3631, 994, 500, 85.0),
			("15:57:10", 0.0122, 101, 12, 1156, 2001, 5531, 5027, 626, 3004, 432, 500, 83.0),
			("15:57:12", 0.0000, 14, 14, 1267, 1999, 5531, 5545, 717, 3472, 44, 256, 83.0),
			("15:57:14", 0.0000, 4, 7, 1283, 1999, 6761, 5604, 382, 1849, 19, 256, 82.0),
			("15:57:16", 0.0069, 23, 15, 1479, 2009, 6322, 6739, 540, 2601, 132, 151, 82.0),
			("15:57:18", 0.0000, 7, 15, 1425, 1990, 6954, 6365, 358, 1741, 9, 151, 83.0),
			("15:57:20", 0.0000, 0, 15, 1533, 2009, 6954, 6968, 362, 1744, 0, 151, 84.0),
			("15:57:22", 0.0000, 0, 15, 1537, 1999, 6954, 7009, 360, 1743, 0, 151, 80.0),
			("15:57:24", 0.0000, 0, 16, 1534, 2000, 8181, 6983, 360, 1742, 0, 151, 83.0),
			("15:57:26", 0.0413, 130, 17, 1801, 1999, 8522, 8155, 360, 1743, 811, 357, 81.0),
			("15:57:28", 0.1165, 245, 16, 1845, 2000, 7305, 8526, 720, 3484, 2067, 500, 84.0),
			("15:57:30", 0.0726, 225, 11, 1621, 2001, 5259, 7316, 889, 4300, 1425, 500, 84.0),
			("15:57:32", 0.0000, 36, 9, 1162, 1992, 5454, 5271, 619, 3007, 111, 500, 81.0),
			("15:57:34", 0.0008, 3, 17, 1266, 1998, 6508, 5528, 711, 3444, 11, 317, 85.0),
			("15:57:36", 0.0007, 44, 23, 1467, 2005, 7437, 6578, 540, 2607, 178, 317, 80.0),
			("15:57:38", 0.0360, 125, 16, 1634, 1999, 7670, 7417, 554, 2682, 754, 355, 81.0),
			("15:57:40", 0.0694, 156, 15, 1668, 2001, 5663, 7679, 725, 3507, 1170, 480, 83.0),
			("15:57:42", 0.0070, 39, 18, 1279, 2002, 5206, 5684, 720, 3481, 157, 480, 83.0),
			("15:57:44", 0.0000, 0, 14, 1148, 2003, 6818, 5220, 605, 2923, 0, 261, 83.0),
			("15:57:46", 0.0054, 24, 11, 1494, 1999, 7670, 6845, 540, 2614, 136, 261, 82.0),
			("15:57:48", 0.3629, 409, 16, 1641, 1989, 6519, 7598, 528, 2569, 6084, 500, 85.0),
			("15:57:50", 0.3259, 434, 19, 1499, 2008, 5794, 6691, 763, 3678, 5064, 500, 78.0),
			("15:57:52", 0.0023, 40, 12, 1293, 1999, 5531, 5863, 723, 3501, 152, 500, 80.0),
			("15:57:54", 0.0016, 18, 12, 1277, 1995, 5531, 5599, 715, 3469, 49, 500, 85.0),
			("15:57:56", 0.0016, 19, 11, 1269, 2000, 5531, 5570, 717, 3470, 72, 500, 79.0),
		];
		let mut p = FecPolicy::new(true);
		for (t, loss, nack_ok, fec_ok, sent_pkts, window_ms, kbps, video, parity_pkts, parity, rtx, old, rtt_ms) in rows {
			let bytes = |k: u64| k * window_ms as u64 / 8;
			let w = FecWindow {
				loss,
				nack_ok,
				fec_ok,
				sent_pkts,
				window_ms,
				kbps,
				fps: 90,
				rtt_ms,
				video_bytes: bytes(video),
				parity_pkts,
				parity_bytes: bytes(parity),
				rtx_bytes: bytes(rtx),
			};
			let r = p.on_window(&w);
			assert!(r <= WAN_FLOOR + 0.05, "{t}: {r} (old policy {} ‰, {})", old, p.hold_reason());
			if p.last_raw_loss() >= 0.15 {
				assert_eq!(r, WAN_FLOOR, "{t}: congestion window ({})", p.hold_reason());
			}
		}
	}

	#[test]
	fn wan_starts_with_parity_and_holds_a_floor_30s_after_loss() {
		let mut p = FecPolicy::new(true);
		assert_eq!(p.ratio(), WAN_FLOOR, "WAN starts protected");
		// Clean WAN: the floor holds for the first 30 s, then off.
		for i in 1..=14 {
			assert_eq!(p.on_window(&win(10_000, 0, 0, 30.0)), WAN_FLOOR, "window {i}");
		}
		assert_eq!(p.on_window(&win(10_000, 0, 0, 30.0)), 0.0, "30 s clean → off");
		// A lossy window: the floor comes back at once and holds for 30 s after that window.
		// (Nothing above it yet: one window is not persisting loss — since the congestion fix;
		// it was sized above the floor at once before.)
		assert_eq!(p.on_window(&win(10_000, 0, 500, 30.0)), WAN_FLOOR);
		let mut seen = Vec::new();
		for _ in 0..14 {
			seen.push(p.on_window(&win(10_000, 0, 0, 30.0)));
		}
		assert!(seen.iter().all(|&x| x >= WAN_FLOOR - 1e-6), "{seen:?}");
		assert_eq!(p.on_window(&win(10_000, 0, 0, 30.0)), 0.0);
	}

	#[test]
	fn a_long_rtt_promotes_an_overlay_lan_address_to_wan() {
		let mut p = FecPolicy::new(false);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 3.0)), 0.0);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 85.0)), 0.0, "one window is not enough");
		assert_eq!(p.on_window(&win(10_000, 0, 0, 85.0)), WAN_FLOOR, "internet-sized RTT");
		assert!(p.is_wan());
	}

	#[test]
	fn a_junk_startup_rtt_does_not_turn_lan_parity_on() {
		// 2026-11-09 LAN run: the first window's RTT read 244 ms on a 4 ms LAN.
		let mut p = FecPolicy::new(false);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 244.0)), 0.0);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 4.0)), 0.0);
		assert!(!p.is_wan());
	}

	#[test]
	fn a_lan_rtt_drops_the_proactive_wan_parity() {
		// Started on the relay (WAN guess), upgraded to a direct LAN path: clean → off at once.
		let mut p = FecPolicy::new(true);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 45.0)), WAN_FLOOR);
		assert_eq!(p.on_window(&win(10_000, 0, 0, 2.0)), 0.0);
		assert!(!p.is_wan());
		// With recent loss the parity stays, sized by the loss under the LAN rules. (Two lossy
		// windows to size it above the floor since the congestion fix; one did before.)
		let mut q = FecPolicy::new(true);
		assert_eq!(q.on_window(&win(10_000, 0, 500, 45.0)), WAN_FLOOR);
		let r = q.on_window(&win(10_000, 0, 500, 45.0));
		assert!(r > WAN_FLOOR, "{r}");
		assert!(q.on_window(&win(10_000, 0, 0, 2.0)) > 0.0, "short clean stretch keeps it");
	}

	#[test]
	fn fps_from_point_labels() {
		assert_eq!(point_label_fps("1080p120"), 120);
		assert_eq!(point_label_fps("720p60"), 60);
		assert_eq!(point_label_fps(""), 0);
		assert_eq!(point_label_fps("junk"), 0);
	}

	#[test]
	fn lan_addresses() {
		use std::net::IpAddr;
		for a in ["192.168.1.61", "10.147.17.3", "172.16.0.9", "127.0.0.1", "169.254.1.1", "fe80::1", "fd00::5", "::1", "::ffff:192.168.1.2"] {
			assert!(is_lan_ip(a.parse::<IpAddr>().unwrap()), "{a}");
		}
		for a in ["2.28.36.116", "100.72.1.1", "8.8.8.8", "2001:db8::1", "::ffff:8.8.8.8"] {
			assert!(!is_lan_ip(a.parse::<IpAddr>().unwrap()), "{a}");
		}
	}
}
