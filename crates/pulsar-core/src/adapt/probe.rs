//! Session-start bandwidth probe: measure the path before the first frame instead of finding
//! it by slow climbing.
//!
//! WHY: the controller used to start cautiously (~4 Mbit) and climb ×1.5 per 2 s window, so a
//! 15 Mbit LAN took 8-10 s to reach its rate and the "preparing" screen (8 s max) revealed the
//! stream mid-climb; on a thin mobile link the climb ran into the policer before it settled.
//! GeForce-Now-style services and WebRTC (probe clusters) measure first instead.
//!
//! HOW: the client asks the host (`DataMsg::ProbeReq`, only to hosts advertising
//! [`FEAT_PROBE`](crate::service::media::FEAT_PROBE)) for a few short packet trains at rising
//! rates ([`plan_rates`]). Each train is `train_ms` long; between trains the host pauses
//! [`GAP_MS`] so a queue built by one train drains (and a policer's bucket refills a little)
//! before the next. The client stamps every probe packet's arrival ([`Meter`]); per train it
//! gets the delivered rate (dispersion: bytes over the first-to-last arrival span), the loss,
//! and the one-way delay growth across the train (host send stamps vs arrival times — the
//! clock offset cancels). The path's rate is the highest train that arrived clean; where the
//! trains stop arriving clean, what the path still delivered bounds the estimate from above
//! ([`Meter::estimate`]). A train that failed while a FASTER one after it arrived clean met a
//! stall, not the path's limit, and does not count (see [`verdict`]).
//!
//! Limits: a policer with a deep bucket (some carriers allow a few hundred KB) lets a short
//! train through above its rate, so the estimate can read high — the trains are long enough
//! (≥ 150 ms, hundreds of KB at the top rates) to drain the common small buckets, and the
//! controller corrects the rest after the start. The probe only picks the START.
//!
//! No I/O here: the host sender lives in `service::media`, the client loop in `service::client`.

/// Payload bytes per probe packet (under the ~1200 B the video packets use, so it fits the
/// same path MTU after the session seal).
pub const PACKET_BYTES: usize = 1100;
/// One train's length.
pub const TRAIN_MS: u16 = 150;
/// Pause between trains.
pub const GAP_MS: u64 = 60;
/// The lowest train rate (kbit/s).
pub const MIN_RATE_KBPS: u32 = 2_500;
/// Host-side bounds on what one request may ask for (an authenticated client could otherwise
/// make the host flood its own path or the relay).
pub const MAX_TRAINS: usize = 6;
pub const MAX_RATE_KBPS: u32 = 120_000;
pub const MAX_TRAIN_MS: u16 = 300;
/// A train kept up with the path when its tail was delivered at ≥ this share of its rate…
/// (Loss alone doesn't fail a train: random loss lowers the delivered rate by only its own
/// share, a capacity limit by far more once the train is well above it.)
const KEPT_UP: f32 = 0.85;
/// …lost no more than this (a heavier loss is no usable path, whatever its cause)…
const MAX_LOSS: f32 = 0.30;
/// …and its one-way delay grew less than this across it (ms): a queue filling at the
/// bottleneck delays the train's tail by (rate / capacity − 1) × its length.
const CLEAN_DELAY_GROWTH_MS: f32 = 20.0;
/// A train needs this many packets back to be judged at all.
const MIN_PACKETS: usize = 5;

/// The train rates (kbit/s) to ask for under a session cap: doubling from
/// [`MIN_RATE_KBPS`] until one passes 1.25 × the cap (no need to measure far beyond what the
/// session may use), at most [`MAX_TRAINS`].
pub fn plan_rates(cap_kbps: u32) -> Vec<u32> {
	let top = ((cap_kbps.max(MIN_RATE_KBPS) as f32) * 1.25) as u32;
	let mut v = Vec::new();
	let mut r = MIN_RATE_KBPS;
	while v.len() < MAX_TRAINS {
		v.push(r.min(top).min(MAX_RATE_KBPS));
		if r >= top || r >= MAX_RATE_KBPS {
			break;
		}
		r = r.saturating_mul(2);
	}
	v.dedup();
	v
}

/// Packets in one train at `rate_kbps` for `train_ms`.
pub fn train_packets(rate_kbps: u32, train_ms: u16) -> u16 {
	let bytes = rate_kbps as u64 * train_ms as u64 / 8;
	(bytes / PACKET_BYTES as u64).clamp(MIN_PACKETS as u64, 4_000) as u16
}

/// The header of one probe packet (after the media tag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr {
	/// Train index in the request.
	pub train: u8,
	/// Packet index in the train and the train's packet count.
	pub idx: u16,
	pub count: u16,
	/// Host send time, µs since the probe began (wraps after ~71 min — irrelevant here).
	pub send_us: u32,
	/// The train's rate (kbit/s), so the client needn't keep the request around.
	pub rate_kbps: u32,
}

/// Header length on the wire.
pub const HDR_LEN: usize = 1 + 2 + 2 + 4 + 4;

impl Hdr {
	pub fn write(&self, out: &mut Vec<u8>) {
		out.push(self.train);
		out.extend_from_slice(&self.idx.to_be_bytes());
		out.extend_from_slice(&self.count.to_be_bytes());
		out.extend_from_slice(&self.send_us.to_be_bytes());
		out.extend_from_slice(&self.rate_kbps.to_be_bytes());
	}

	pub fn parse(b: &[u8]) -> Option<Self> {
		if b.len() < HDR_LEN {
			return None;
		}
		Some(Self {
			train: b[0],
			idx: u16::from_be_bytes([b[1], b[2]]),
			count: u16::from_be_bytes([b[3], b[4]]),
			send_us: u32::from_be_bytes([b[5], b[6], b[7], b[8]]),
			rate_kbps: u32::from_be_bytes([b[9], b[10], b[11], b[12]]),
		})
	}
}

/// What one train showed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrainStat {
	pub rate_kbps: u32,
	pub sent: u16,
	pub received: u16,
	/// Delivered rate over the arrival span (kbit/s; 0 = too few packets to tell).
	pub delivered_kbps: u32,
	pub loss: f32,
	/// One-way delay growth from the train's first to its last packet (ms).
	pub delay_growth_ms: f32,
	pub clean: bool,
}

impl TrainStat {
	/// A train's numbers with the verdict on it (`clean`).
	fn judged(rate_kbps: u32, sent: u16, received: u16, delivered_kbps: u32, delay_growth_ms: f32) -> Self {
		let loss = if sent == 0 { 1.0 } else { 1.0 - received.min(sent) as f32 / sent as f32 };
		let clean = received as usize >= MIN_PACKETS
			&& loss <= MAX_LOSS
			&& (delivered_kbps == 0 || delivered_kbps as f32 >= KEPT_UP * rate_kbps as f32)
			&& delay_growth_ms < CLEAN_DELAY_GROWTH_MS;
		Self {
			rate_kbps,
			sent,
			received: received.min(sent),
			delivered_kbps,
			loss,
			delay_growth_ms,
			clean,
		}
	}
}

/// The probe's verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
	/// The path's usable rate (kbit/s).
	pub kbps: u32,
	/// Every train passed: the path carries at least the top rate (the estimate is a floor).
	pub at_least: bool,
	pub trains: Vec<TrainStat>,
}

#[derive(Clone, Default)]
struct Acc {
	rate_kbps: u32,
	count: u16,
	/// (send µs, arrival ms, bytes) of every packet that arrived (≤ a few thousand).
	pkts: Vec<(u32, f64, usize)>,
}

/// Client-side collector: feed every probe packet, then ask for the [`Estimate`].
#[derive(Default)]
pub struct Meter {
	trains: Vec<Acc>,
}

impl Meter {
	pub fn new() -> Self {
		Self::default()
	}

	/// One probe packet (`len` = its whole payload) that arrived at `arrival_ms`.
	pub fn on_packet(&mut self, h: &Hdr, len: usize, arrival_ms: f64) {
		let i = h.train as usize;
		if i >= MAX_TRAINS {
			return;
		}
		if self.trains.len() <= i {
			self.trains.resize(i + 1, Acc::default());
		}
		let a = &mut self.trains[i];
		a.rate_kbps = h.rate_kbps;
		a.count = h.count;
		if a.pkts.len() < 8_000 {
			a.pkts.push((h.send_us, arrival_ms, len));
		}
	}

	/// The last train's last packet has arrived (the client can stop waiting).
	pub fn complete(&self, trains: usize) -> bool {
		self.trains.len() >= trains && self.trains.last().is_some_and(|a| a.pkts.len() >= a.count as usize && a.count > 0)
	}

	/// Delivered rate (kbit/s) over packets' arrival span; the first arrival opens the span, so
	/// its bytes don't count. 0 = too few packets.
	fn delivered(pkts: &[(u32, f64, usize)]) -> u32 {
		if pkts.len() < MIN_PACKETS {
			return 0;
		}
		let (lo, hi) = pkts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
		let span = hi - lo;
		if span <= 0.5 {
			return 0;
		}
		let first_len = pkts.iter().find(|p| p.1 == lo).map_or(0, |p| p.2);
		let bytes: usize = pkts.iter().map(|p| p.2).sum::<usize>() - first_len;
		((bytes as f64 * 8.0 / span) as u32).max(1)
	}

	fn stat(a: &Acc) -> TrainStat {
		// The rate comes from the train's second half (by send time): a policer's bucket — which
		// refilled during the gap — lets the head of a train through above its rate (a 10 Mbit
		// policer with a 60-packet bucket read 13.6 Mbit over a whole 150 ms train); by the
		// second half the bucket is spent and only the policer's (or a shaper's) rate passes.
		let (s_lo, s_hi) = a.pkts.iter().fold((u32::MAX, 0u32), |(lo, hi), p| (lo.min(p.0), hi.max(p.0)));
		let mid = s_lo.saturating_add(s_hi.saturating_sub(s_lo) / 2);
		let tail: Vec<_> = a.pkts.iter().copied().filter(|p| p.0 >= mid).collect();
		let delivered_kbps = match Self::delivered(&tail) {
			0 => Self::delivered(&a.pkts),
			k => k,
		};
		let first = a.pkts.iter().min_by_key(|p| p.0);
		let last = a.pkts.iter().max_by_key(|p| p.0);
		let delay_growth_ms = match (first, last) {
			(Some(&(s0, r0, _)), Some(&(s1, r1, _))) => ((r1 - r0) - (s1.wrapping_sub(s0) as f64 / 1000.0)) as f32,
			_ => 0.0,
		};
		TrainStat::judged(a.rate_kbps, a.count, a.pkts.len().min(u16::MAX as usize) as u16, delivered_kbps, delay_growth_ms)
	}

	/// The verdict, `None` when nothing usable came back (no host answer / everything lost).
	/// See [`verdict`].
	pub fn estimate(&self) -> Option<Estimate> {
		verdict(self.trains.iter().filter(|a| a.count > 0).map(Self::stat).collect())
	}
}

/// The path's rate from the trains, in the order they were sent (rising rates).
///
/// Walk them: every clean one raises the floor to its rate.
///
/// A train that is NOT clean while a later one at its rate or above IS clean met a stall, not
/// the path's limit — at a real limit every faster train queues or loses more, never less. It
/// is skipped. (2026-11-13, a laptop on Wi-Fi a few metres from its access point: the first
/// train of a session arrived 79 ms late — the station's radio was asleep or off-channel when
/// the session opened — while the 5, 10, 20 and 21 Mbit trains after it arrived whole. The
/// estimate read 1.9 Mbit, and the stream started at 1080p30 on a 20 Mbit path.)
///
/// From the first train that no later one overrules, the path is past its limit, and what it
/// still delivered is the estimate: the HIGHEST (tail) delivered rate among these trains, none
/// counted above its own rate. A shaper drains at its rate and a policer passes its rate
/// whatever the train asked for, so at a real limit they all read the same; where the trains
/// were stretched by stalls instead (the same Wi-Fi: every train's tail 77 ms late at ANY
/// rate, 2.1 Mbit delivered of 2.5 and 8.4 of 20), the first one only says how small it was.
/// A QUEUE (delay grew) keeps the last clean rate as a floor: a deep queue can stretch the
/// span below the real rate. A policer (loss, no queue) does not: its bucket can carry a
/// smaller train through whole above its rate (a 3 Mbit policer passed a 5 Mbit train), so
/// that "clean" rate is no floor. All clean (or overruled) = at least the top clean rate.
pub fn verdict(trains: Vec<TrainStat>) -> Option<Estimate> {
	if trains.is_empty() {
		return None;
	}
	let mut floor = 0u32;
	for (i, t) in trains.iter().enumerate() {
		if t.clean {
			floor = floor.max(t.rate_kbps);
			continue;
		}
		if trains[i + 1..].iter().any(|l| l.clean && l.rate_kbps >= t.rate_kbps) {
			continue;
		}
		let queued = t.delay_growth_ms >= CLEAN_DELAY_GROWTH_MS;
		let delivered = trains[i..].iter().map(|l| l.delivered_kbps.min(l.rate_kbps)).max().unwrap_or(0);
		let delivered = if delivered > 0 { delivered } else { floor };
		let kbps = if queued { delivered.max(floor) } else { delivered };
		if kbps == 0 {
			return None;
		}
		return Some(Estimate { kbps, at_least: false, trains });
	}
	if floor == 0 {
		return None;
	}
	Some(Estimate { kbps: floor, at_least: true, trains })
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A path model: packets of each train are sent evenly; the bottleneck either queues
	/// (`shaper_kbps`, unbounded queue) or drops above its rate (`policer_kbps`, a token bucket
	/// `bucket` bytes deep that refills during the gaps). Returns the meter.
	fn run(rates: &[u32], shaper_kbps: Option<u32>, policer: Option<(u32, f64)>, base_ms: f64) -> Meter {
		run_lossy(rates, shaper_kbps, policer, base_ms, 0)
	}

	/// `drop_every` > 0 drops every that-many-th packet (random loss stand-in).
	fn run_lossy(rates: &[u32], shaper_kbps: Option<u32>, policer: Option<(u32, f64)>, base_ms: f64, drop_every: u32) -> Meter {
		let mut m = Meter::new();
		let mut t_send = 0.0f64; // ms
		let mut queue_free_at = 0.0f64;
		let mut tokens = policer.map(|p| p.1).unwrap_or(0.0);
		let mut tok_at = 0.0f64;
		for (ti, &rate) in rates.iter().enumerate() {
			let n = train_packets(rate, TRAIN_MS);
			let gap = TRAIN_MS as f64 / n as f64;
			for i in 0..n {
				let send = t_send + i as f64 * gap;
				let len = PACKET_BYTES + 1 + HDR_LEN;
				if drop_every > 0 && (i as u32 + ti as u32 * 7) % drop_every == 0 {
					continue;
				}
				if let Some((pk, bucket)) = policer {
					tokens = (tokens + (send - tok_at) * pk as f64 / 8.0).min(bucket);
					tok_at = send;
					if tokens < len as f64 {
						continue;
					}
					tokens -= len as f64;
				}
				let mut arrive = send + base_ms;
				if let Some(sk) = shaper_kbps {
					let tx = len as f64 * 8.0 / sk as f64;
					let start = queue_free_at.max(send);
					queue_free_at = start + tx;
					arrive = queue_free_at + base_ms;
				}
				let h = Hdr { train: ti as u8, idx: i, count: n, send_us: (send * 1000.0) as u32, rate_kbps: rate };
				m.on_packet(&h, len, arrive);
			}
			t_send += TRAIN_MS as f64 + GAP_MS as f64;
		}
		m
	}

	#[test]
	fn plan_doubles_up_to_just_past_the_cap() {
		assert_eq!(plan_rates(15_000), vec![2_500, 5_000, 10_000, 18_750]);
		assert_eq!(plan_rates(4_000), vec![2_500, 5_000]);
		assert!(plan_rates(500_000).len() <= MAX_TRAINS);
		assert!(plan_rates(500_000).iter().all(|&r| r <= MAX_RATE_KBPS));
	}

	#[test]
	fn header_round_trips() {
		let h = Hdr { train: 3, idx: 517, count: 900, send_us: 123_456_789, rate_kbps: 18_750 };
		let mut v = Vec::new();
		h.write(&mut v);
		assert_eq!(v.len(), HDR_LEN);
		assert_eq!(Hdr::parse(&v), Some(h));
		assert_eq!(Hdr::parse(&v[..HDR_LEN - 1]), None);
	}

	#[test]
	fn a_fast_lan_passes_every_train() {
		let rates = plan_rates(15_000);
		let e = run(&rates, None, None, 2.0).estimate().unwrap();
		assert!(e.at_least);
		assert_eq!(e.kbps, 18_750);
	}

	#[test]
	fn a_10_mbit_policer_reads_near_its_rate() {
		// 10 Mbit policer with a ~60-packet bucket (the emulated mobile link of 2026-11-09).
		let rates = plan_rates(15_000);
		let e = run(&rates, None, Some((10_000, 60.0 * 1150.0)), 39.0).estimate().unwrap();
		assert!(!e.at_least, "{e:?}");
		assert!((8_500..=11_500).contains(&e.kbps), "{e:?}");
	}

	#[test]
	fn a_10_mbit_shaper_reads_near_its_rate() {
		let rates = plan_rates(15_000);
		let e = run(&rates, Some(10_000), None, 39.0).estimate().unwrap();
		assert!(!e.at_least, "{e:?}");
		assert!((9_000..=11_000).contains(&e.kbps), "{e:?}");
	}

	#[test]
	fn a_3_mbit_link_is_not_read_as_more() {
		let rates = plan_rates(15_000);
		let e = run(&rates, None, Some((3_000, 60.0 * 1150.0)), 39.0).estimate().unwrap();
		assert!(e.kbps <= 3_600, "{e:?}");
		assert!(e.kbps >= 2_500, "{e:?}");
	}

	#[test]
	fn random_loss_on_a_fast_link_is_not_read_as_a_limit() {
		// ~8 % random loss, no bottleneck: the trains still keep up with their rates.
		let rates = plan_rates(15_000);
		let e = run_lossy(&rates, None, None, 39.0, 12).estimate().unwrap();
		assert!(e.kbps >= 15_000, "{e:?}");
	}

	#[test]
	fn nothing_back_is_no_estimate() {
		assert_eq!(Meter::new().estimate(), None);
	}

	/// `(rate, delivered, delay growth ms)` per train, no loss — as the client logs them.
	fn logged(trains: &[(u32, u32, f32)]) -> Vec<TrainStat> {
		trains
			.iter()
			.map(|&(rate, delivered, growth)| {
				let n = train_packets(rate, TRAIN_MS);
				TrainStat::judged(rate, n, n, delivered, growth)
			})
			.collect()
	}

	#[test]
	fn a_stall_on_the_first_train_is_not_the_paths_limit() {
		// 2026-11-13 20:19:30, LAN Wi-Fi: the old walk stopped at the first train → 1.9 Mbit.
		let e = verdict(logged(&[
			(2_500, 1_943, 79.0),
			(5_000, 4_966, -16.0),
			(10_000, 10_015, 0.0),
			(20_000, 20_289, -2.0),
			(21_021, 21_730, 0.0),
		]))
		.unwrap();
		assert!(e.at_least, "{e:?}");
		assert_eq!(e.kbps, 21_021);
		// Two stalled trains, then the path shows itself (19:08:12: read 1.3 Mbit).
		let e = verdict(logged(&[
			(2_500, 1_281, 91.0),
			(5_000, 2_046, 78.0),
			(10_000, 10_242, -46.0),
			(20_000, 20_317, -63.0),
			(21_021, 23_793, -83.0),
		]))
		.unwrap();
		assert!(e.at_least && e.kbps == 21_021, "{e:?}");
	}

	#[test]
	fn a_stall_in_the_middle_is_skipped_too() {
		// 18:02:30: the 10 Mbit train came up short, the 20 and 21 Mbit ones did not.
		let e = verdict(logged(&[
			(2_500, 3_859, 0.0),
			(5_000, 5_006, -7.0),
			(10_000, 8_073, 14.0),
			(20_000, 18_109, 8.0),
			(21_021, 24_720, 2.0),
		]))
		.unwrap();
		assert!(e.at_least && e.kbps == 21_021, "{e:?}");
	}

	#[test]
	fn trains_stretched_by_stalls_read_what_the_path_delivered() {
		// 19:25:52: every train's tail ~77 ms late at any rate. No capacity limit looks like
		// that (the delay would grow with the rate, the delivered rate would not) — the old
		// walk read 2.1 Mbit from the first train; the path delivered 8.4.
		let e = verdict(logged(&[
			(2_500, 2_154, 56.0),
			(5_000, 2_520, 73.0),
			(10_000, 4_703, 77.0),
			(20_000, 8_396, 77.0),
			(21_021, 8_238, 76.0),
		]))
		.unwrap();
		assert!(!e.at_least, "{e:?}");
		assert_eq!(e.kbps, 8_396);
	}

	#[test]
	fn a_real_limit_is_still_read_at_the_first_train_past_it() {
		// A 6 Mbit shaper: 2.5 and 5 pass, 10 / 20 drain at 6 with a growing queue.
		let e = verdict(logged(&[(2_500, 2_600, 0.0), (5_000, 5_050, 1.0), (10_000, 6_010, 100.0), (20_000, 5_990, 350.0)])).unwrap();
		assert!(!e.at_least, "{e:?}");
		assert!((5_900..=6_100).contains(&e.kbps), "{e:?}");
		// The last train alone comes up short (nothing after it to overrule it): the last
		// clean rate holds as the floor.
		let e = verdict(logged(&[(2_500, 2_600, 0.0), (5_000, 5_050, 1.0), (10_000, 10_010, 0.0), (20_000, 9_000, 77.0)])).unwrap();
		assert!(!e.at_least && e.kbps == 10_000, "{e:?}");
	}

	#[test]
	fn complete_when_the_last_trains_tail_is_in() {
		let rates = plan_rates(4_000);
		let m = run(&rates, None, None, 2.0);
		assert!(m.complete(rates.len()));
		assert!(!Meter::new().complete(1));
	}
}
