//! Client-side audio repair, shared by every client (desktop `native_audio`, mobile): puts the
//! host's audio RTP datagrams back in sequence order, rebuilds lost ones from the host's
//! Reed-Solomon parity ([`TAG_AUDIO_FEC`](crate::service::media::TAG_AUDIO_FEC)) and only then
//! reports a loss to the decoder (hidden-gaps 1.11a).
//!
//! Before this, a packet one step late (Wi-Fi reorders) was dropped as "late" after the one
//! behind it had already been concealed: a reorder cost a PLC'd frame AND the real one. Now a
//! hole is held open for `hold` (the caller's choice, e.g. what its output queue can cover)
//! while later packets wait behind it; the missing packet arriving late, or its parity
//! rebuilding it, fills the hole in order. Past `hold` the hole is reported as lost and the
//! waiting packets play. No extra delay without a hole: in-order packets pass at once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::service::media::{parse_rs_parity, RsFecDecoder, FEC_MAX_PACKET};

/// What the decoder gets, in play order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repaired {
	/// The next datagram (RTP, as received or rebuilt).
	Packet(Vec<u8>),
	/// This many datagrams are gone for good: conceal them (or restart past a long gap).
	Lost(u16),
	/// A new stream (the host restarted its encoder, or a jump too far to conceal): reset the
	/// decoder before the next packet.
	Restart,
}

/// Counters since the start (the caller diffs them for its stats line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepairStats {
	/// Packets rebuilt from parity that were then played.
	pub recovered: u32,
	/// Packets that arrived after their slot was already played or given up (dropped).
	pub late: u32,
	/// Packets that came twice (dropped).
	pub dup: u32,
	/// Packets reported lost.
	pub lost: u32,
}

/// A packet this far ahead of the expected one (or this far behind) is a new stream.
const JUMP: u16 = 64;
/// Most packets waiting behind one hole; past this the hole is given up at once.
const MAX_HELD: usize = 32;
/// Largest parity block shape accepted (`audio_fec_shape`: Opus 2+1, L16 4+2). Anything else is
/// not audio parity and is dropped before the decoder stores its shards.
const MAX_PARITY_K: u8 = 4;
const MAX_PARITY_M: u8 = 2;

/// See the module docs.
#[derive(Debug)]
pub struct AudioRepair {
	fec: RsFecDecoder,
	next: Option<u16>,
	/// Packets ahead of `next`, sorted by sequence distance from it, with their arrival time.
	/// The hole in front of them falls due `hold` after the EARLIEST of those arrivals, so no
	/// packet waits longer than `hold` (and a second hole gets its own time once the first
	/// one fills, from the packets behind it).
	held: VecDeque<(u16, Vec<u8>, Instant)>,
	out: VecDeque<Repaired>,
	stats: RepairStats,
}

impl Default for AudioRepair {
	fn default() -> Self {
		Self::new()
	}
}

impl AudioRepair {
	pub fn new() -> Self {
		Self {
			fec: RsFecDecoder::new(64),
			next: None,
			held: VecDeque::new(),
			out: VecDeque::new(),
			stats: RepairStats::default(),
		}
	}

	pub fn stats(&self) -> RepairStats {
		self.stats
	}

	/// An audio RTP datagram arrived (the body of a `TAG_AUDIO` frame, or a plain UDP one).
	pub fn push(&mut self, rtp: Vec<u8>, now: Instant) {
		let Some(seq) = rtp_seq(&rtp) else { return };
		self.fec.on_packet(seq, &rtp);
		self.place(seq, rtp, now, false);
	}

	/// An audio parity datagram arrived (the body after `TAG_AUDIO_FEC`). Only audio block
	/// shapes are taken: the decoder keeps shards per block, so an arbitrary (k, m, length)
	/// from whoever can reach the port would otherwise cost memory.
	pub fn push_parity(&mut self, body: &[u8], now: Instant) {
		let Some(h) = parse_rs_parity(body) else { return };
		if h.k > MAX_PARITY_K || h.m > MAX_PARITY_M || h.shard_len as usize > FEC_MAX_PACKET + 2 {
			return;
		}
		for (seq, pkt) in self.fec.on_parity(body) {
			self.place(seq, pkt, now, true);
		}
	}

	/// The next thing to play, if any is due: in-order packets at once, a hole once it has
	/// been open for `hold` (then [`Repaired::Lost`], followed by the packets behind it).
	pub fn pop(&mut self, now: Instant, hold: Duration) -> Option<Repaired> {
		if self.out.is_empty() {
			self.release(now, hold);
		}
		self.out.pop_front()
	}

	/// When the open hole falls due (`None`: no hole), so a caller blocked on its input can
	/// wake up for it.
	pub fn deadline(&self, hold: Duration) -> Option<Instant> {
		self.hole_since().map(|t| t + hold)
	}

	/// When the open hole started to delay a packet: the earliest arrival behind it.
	fn hole_since(&self) -> Option<Instant> {
		self.held.iter().map(|(_, _, t)| *t).min()
	}

	fn place(&mut self, seq: u16, pkt: Vec<u8>, now: Instant, rebuilt: bool) {
		let Some(next) = self.next else {
			self.next = Some(seq);
			self.insert(0, seq, pkt, now, rebuilt);
			self.drain_in_order(now);
			return;
		};
		let ahead = seq.wrapping_sub(next);
		if ahead >= 0x8000 {
			if next.wrapping_sub(seq) <= JUMP {
				// Its slot was played or given up already.
				if !rebuilt {
					self.stats.late += 1;
				}
				return;
			}
			self.restart(seq, pkt, now, rebuilt);
			return;
		}
		if ahead > JUMP {
			self.restart(seq, pkt, now, rebuilt);
			return;
		}
		self.insert(ahead, seq, pkt, now, rebuilt);
		self.drain_in_order(now);
	}

	fn restart(&mut self, seq: u16, pkt: Vec<u8>, now: Instant, rebuilt: bool) {
		// Whatever waited behind a hole of the old stream still plays first, in order.
		while let Some((_, p, _)) = self.held.pop_front() {
			self.out.push_back(Repaired::Packet(p));
		}
		self.out.push_back(Repaired::Restart);
		self.next = Some(seq);
		self.insert(0, seq, pkt, now, rebuilt);
		self.drain_in_order(now);
	}

	fn insert(&mut self, ahead: u16, seq: u16, pkt: Vec<u8>, now: Instant, rebuilt: bool) {
		let next = self.next.unwrap_or(seq);
		let pos = self.held.iter().position(|(s, _, _)| s.wrapping_sub(next) >= ahead);
		match pos {
			Some(i) if self.held[i].0 == seq => {
				if !rebuilt {
					self.stats.dup += 1;
				}
			}
			Some(i) => {
				self.held.insert(i, (seq, pkt, now));
				if rebuilt {
					self.stats.recovered += 1;
				}
			}
			None => {
				self.held.push_back((seq, pkt, now));
				if rebuilt {
					self.stats.recovered += 1;
				}
			}
		}
	}

	/// Move every packet that is next in line to the output.
	fn drain_in_order(&mut self, now: Instant) {
		while let (Some(next), Some((s, _, _))) = (self.next, self.held.front()) {
			if *s != next {
				break;
			}
			let (_, p, _) = self.held.pop_front().expect("front exists");
			self.out.push_back(Repaired::Packet(p));
			self.next = Some(next.wrapping_add(1));
		}
		if self.held.len() > MAX_HELD {
			self.give_up(now);
		}
	}

	fn release(&mut self, now: Instant, hold: Duration) {
		if self.hole_since().is_some_and(|t| now.saturating_duration_since(t) >= hold) {
			self.give_up(now);
		}
	}

	/// Report the hole in front of the first waiting packet as lost and play on from it.
	fn give_up(&mut self, now: Instant) {
		let (Some(next), Some((s, _, _))) = (self.next, self.held.front()) else { return };
		let lost = s.wrapping_sub(next);
		if lost > 0 {
			self.stats.lost += lost as u32;
			self.out.push_back(Repaired::Lost(lost));
		}
		self.next = Some(*s);
		self.drain_in_order(now);
	}
}

fn rtp_seq(rtp: &[u8]) -> Option<u16> {
	(rtp.len() >= 12 && rtp[0] >> 6 == 2).then(|| u16::from_be_bytes([rtp[2], rtp[3]]))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::service::media::AudioFecEncoder;

	fn pkt(seq: u16) -> Vec<u8> {
		let mut v = vec![0u8; 40];
		v[0] = 0x80;
		v[1] = crate::audio::OPUS_PAYLOAD_TYPE;
		v[2..4].copy_from_slice(&seq.to_be_bytes());
		v[8] = 3;
		v[20] = seq as u8;
		v
	}

	fn all(r: &mut AudioRepair, now: Instant, hold: Duration) -> Vec<Repaired> {
		std::iter::from_fn(|| r.pop(now, hold)).collect()
	}

	const HOLD: Duration = Duration::from_millis(20);

	#[test]
	fn in_order_packets_pass_at_once() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		for s in [65534u16, 65535, 0, 1] {
			r.push(pkt(s), t);
		}
		assert_eq!(all(&mut r, t, HOLD), [65534u16, 65535, 0, 1].map(|s| Repaired::Packet(pkt(s))).to_vec());
		assert_eq!(r.deadline(HOLD), None);
	}

	#[test]
	fn a_reordered_packet_fills_its_hole() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		r.push(pkt(10), t);
		r.push(pkt(12), t);
		assert_eq!(all(&mut r, t, HOLD), vec![Repaired::Packet(pkt(10))], "12 waits behind the hole");
		assert_eq!(r.deadline(HOLD), Some(t + HOLD));
		r.push(pkt(11), t + Duration::from_millis(4));
		assert_eq!(all(&mut r, t + Duration::from_millis(4), HOLD), vec![Repaired::Packet(pkt(11)), Repaired::Packet(pkt(12))]);
		assert_eq!(r.stats(), RepairStats::default());
	}

	#[test]
	fn a_hole_past_the_hold_is_lost_and_its_packet_late() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		r.push(pkt(1), t);
		r.push(pkt(3), t);
		r.push(pkt(4), t);
		let _ = all(&mut r, t, HOLD);
		let later = t + HOLD;
		assert_eq!(all(&mut r, later, HOLD), vec![Repaired::Lost(1), Repaired::Packet(pkt(3)), Repaired::Packet(pkt(4))]);
		r.push(pkt(2), later);
		assert!(all(&mut r, later, HOLD).is_empty());
		assert_eq!(r.stats(), RepairStats { late: 1, lost: 1, ..Default::default() });
		// A zero hold conceals at once (a caller with nothing queued).
		r.push(pkt(6), later);
		assert_eq!(all(&mut r, later, Duration::ZERO), vec![Repaired::Lost(1), Repaired::Packet(pkt(6))]);
	}

	#[test]
	fn parity_rebuilds_a_loss_inside_the_hold() {
		let t = Instant::now();
		let mut r = AudioRepair::new();
		let mut enc = AudioFecEncoder::new();
		let p: Vec<Vec<u8>> = (200..204).map(pkt).collect();
		let mut parity = Vec::new();
		for x in &p {
			parity.extend(enc.push(x));
		}
		r.push(p[0].clone(), t);
		r.push(p[1].clone(), t);
		// 202 lost, 203 arrives, then the block's parity.
		r.push(p[3].clone(), t);
		assert_eq!(all(&mut r, t, HOLD), vec![Repaired::Packet(p[0].clone()), Repaired::Packet(p[1].clone())]);
		r.push_parity(&parity[1][1..], t + Duration::from_millis(2));
		assert_eq!(
			all(&mut r, t + Duration::from_millis(2), HOLD),
			vec![Repaired::Packet(p[2].clone()), Repaired::Packet(p[3].clone())]
		);
		assert_eq!(r.stats().recovered, 1);
		assert_eq!(r.stats().lost, 0);
	}

	#[test]
	fn a_second_hole_gets_its_own_hold() {
		// Holes at 11 and 13: 12 waits from t, 14 from t + 10 ms. 11 fills at t + 18 ms; hole
		// 13 is then timed from 14's arrival (due t + 30 ms), not from 12's.
		let mut r = AudioRepair::new();
		let t = Instant::now();
		let ms = |n: u64| t + Duration::from_millis(n);
		r.push(pkt(10), t);
		r.push(pkt(12), t);
		r.push(pkt(14), ms(10));
		let _ = all(&mut r, ms(10), HOLD);
		r.push(pkt(11), ms(18));
		assert_eq!(all(&mut r, ms(18), HOLD), vec![Repaired::Packet(pkt(11)), Repaired::Packet(pkt(12))]);
		assert_eq!(r.deadline(HOLD), Some(ms(30)));
		r.push(pkt(13), ms(25));
		assert_eq!(all(&mut r, ms(25), HOLD), vec![Repaired::Packet(pkt(13)), Repaired::Packet(pkt(14))]);
		assert_eq!(r.stats(), RepairStats::default());
	}

	#[test]
	fn no_packet_waits_longer_than_the_hold() {
		// 13 arrives first, then 12: the hole at 11 is due `hold` after 13, the earliest.
		let mut r = AudioRepair::new();
		let t = Instant::now();
		r.push(pkt(10), t);
		r.push(pkt(13), t);
		r.push(pkt(12), t + Duration::from_millis(15));
		let _ = all(&mut r, t, HOLD);
		assert_eq!(r.deadline(HOLD), Some(t + HOLD));
		assert_eq!(
			all(&mut r, t + HOLD, HOLD),
			vec![Repaired::Lost(1), Repaired::Packet(pkt(12)), Repaired::Packet(pkt(13))]
		);
	}

	#[test]
	fn parity_that_is_not_an_audio_shape_is_dropped() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		// k = 200, m = 50, 60 000-byte shard: never stored.
		let mut body = vec![0u8, 1, 200, 50, 0];
		body.extend(60_000u16.to_be_bytes());
		body.extend(vec![0u8; 60_000]);
		r.push_parity(&body, t);
		assert_eq!(r.fec.last_ratio(), 0.0, "the decoder never saw a non-audio shape");
		r.push(pkt(1), t);
		assert_eq!(all(&mut r, t, HOLD), vec![Repaired::Packet(pkt(1))]);
	}

	#[test]
	fn duplicates_drop_and_a_new_stream_restarts() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		r.push(pkt(7), t);
		r.push(pkt(7), t);
		assert_eq!(all(&mut r, t, HOLD), vec![Repaired::Packet(pkt(7))]);
		assert_eq!(r.stats().late, 1, "a repeat of a played packet is late");
		r.push(pkt(9), t);
		r.push(pkt(9), t);
		assert_eq!(r.stats().dup, 1, "a repeat of a waiting packet is a duplicate");
		// The host restarted its encoder: random new numbers far away.
		r.push(pkt(40_000), t);
		assert_eq!(
			all(&mut r, t, HOLD),
			vec![Repaired::Packet(pkt(9)), Repaired::Restart, Repaired::Packet(pkt(40_000))]
		);
		r.push(pkt(40_001), t);
		assert_eq!(all(&mut r, t, HOLD), vec![Repaired::Packet(pkt(40_001))]);
	}

	#[test]
	fn a_long_queue_behind_a_hole_gives_up_without_waiting() {
		let mut r = AudioRepair::new();
		let t = Instant::now();
		r.push(pkt(0), t);
		for s in 2..(2 + MAX_HELD as u16 + 1) {
			r.push(pkt(s), t);
		}
		let got = all(&mut r, t, Duration::from_secs(10));
		assert_eq!(got[1], Repaired::Lost(1));
		assert_eq!(got.len(), 2 + MAX_HELD + 1);
	}
}
