//! Media-over-session framing: RTP datagrams (video + audio) carried INSIDE the
//! encrypted session instead of as separate plain UDP flows to extra ports.
//!
//! Why: one external UDP socket per device — the session's hole-punched/relayed
//! path is the ONLY hole anyone needs (symmetric NAT then works via the relay,
//! and self-hosters open exactly one port). The media also becomes end-to-end
//! encrypted for free (it rides the session's ChaCha20-Poly1305 seal).
//!
//! Framing: one session payload per RTP datagram, `[tag][rtp…]`. The service's
//! control messages are JSON (first byte `{` = 0x7B), so a 0x01/0x02 tag byte can
//! never collide with them. Loss/reorder semantics are unchanged — the session
//! transport is still plain UDP underneath, RTP's own seq/jitter handling stays
//! in charge (plus the optional NACK retransmit, see `DataMsg::MediaNack`).

/// Tag byte: a video RTP datagram follows.
pub const TAG_VIDEO: u8 = 0x01;
/// Tag byte: an audio (Opus) RTP datagram follows.
pub const TAG_AUDIO: u8 = 0x02;
// (`TAG_FEC` = 0x03, the XOR parity datagram, is defined with the FEC code below.)

/// Feature ids a host advertises in `StreamCaps::features`:
/// `mos` = media-over-session supported; `nack` = it honors `MediaNack` retransmits.
pub const FEAT_MOS: &str = "mos";
pub const FEAT_NACK: &str = "nack";

/// Build one media session-payload: `[tag][rtp…]`.
pub fn frame(tag: u8, rtp: &[u8]) -> Vec<u8> {
	let mut v = Vec::with_capacity(1 + rtp.len());
	v.push(tag);
	v.extend_from_slice(rtp);
	v
}

/// Parse a session payload as a media frame; `None` for anything else (JSON
/// control messages, junk). Empty RTP is rejected.
pub fn parse(payload: &[u8]) -> Option<(u8, &[u8])> {
	match payload.split_first() {
		Some((&tag @ (TAG_VIDEO | TAG_AUDIO | TAG_FEC), rest)) if !rest.is_empty() => Some((tag, rest)),
		_ => None,
	}
}

/// The RTP sequence number of a datagram (bytes 2..4, big-endian), used for the
/// client's gap detection (NACK + loss accounting). `None` if too short.
pub fn rtp_seq(rtp: &[u8]) -> Option<u16> {
	if rtp.len() < 4 {
		return None;
	}
	Some(u16::from_be_bytes([rtp[2], rtp[3]]))
}

/// Forward-distance from `a` to `b` in u16 sequence space (wrap-aware): how many
/// steps forward `b` is from `a`. Values ≥ 0x8000 mean `b` is actually BEHIND `a`
/// (an old/reordered packet).
pub fn seq_forward(a: u16, b: u16) -> u16 {
	b.wrapping_sub(a)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn frame_parse_roundtrip() {
		let rtp = [0x80u8, 96, 0x12, 0x34, 0, 0, 0, 0];
		let f = frame(TAG_VIDEO, &rtp);
		let (tag, body) = parse(&f).expect("parses");
		assert_eq!(tag, TAG_VIDEO);
		assert_eq!(body, &rtp);
	}

	#[test]
	fn parse_rejects_control_and_junk() {
		assert!(parse(b"{\"Ping\":null}").is_none(), "JSON is not media");
		assert!(parse(&[]).is_none());
		assert!(parse(&[TAG_VIDEO]).is_none(), "empty RTP rejected");
		assert!(parse(&[0x07, 1, 2, 3]).is_none(), "unknown tag rejected");
	}

	#[test]
	fn rtp_seq_and_wraparound() {
		let rtp = [0x80u8, 96, 0xFF, 0xFE, 0, 0, 0, 0];
		assert_eq!(rtp_seq(&rtp), Some(0xFFFE));
		assert_eq!(rtp_seq(&[1, 2, 3]), None);
		assert_eq!(seq_forward(0xFFFE, 0x0001), 3, "wraps forward");
		assert!(seq_forward(5, 2) >= 0x8000, "behind reads as huge forward");
	}
}

// ── Forward error correction: XOR parity over the media-over-session video flow ───────
//
// Adaptive streaming Phase 2.1. On a path where RTT makes NACK too slow, the only repair
// that works is one that needs **zero round-trips**: every `n` consecutive video datagrams
// the host also sends one parity datagram — the XOR of the `n` packets (each padded to the
// longest) plus the XOR of their lengths. A client that has all but ONE of the group's
// packets rebuilds the missing one from the parity, exactly, and feeds it to the renderer as
// if it had arrived (late, so it lands in the reorder buffer). Cost: 1/n extra bandwidth,
// sized by `adapt::fec_policy` from the loss the client reports; off on a clean path.
//
// Framing: `[TAG_FEC][base_seq u16 BE][n u8][len_xor u16 BE][parity bytes…]` — a session
// payload like the other media tags. An old client's `parse` rejects the tag → ignored.

/// Tag byte: an XOR parity datagram over the preceding video packets follows.
pub const TAG_FEC: u8 = 0x03;
/// Feature id: the host can send FEC parity (the client still has to ask, `StreamReq::fec`).
pub const FEAT_FEC: &str = "fec";

/// Largest RTP datagram a parity group covers (packets are padded to the longest).
pub const FEC_MAX_PACKET: usize = 1500;

/// A parsed parity datagram (borrowed from the session payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parity<'a> {
	/// First sequence number of the group; the group is `base..base+n` (wrap-aware).
	pub base_seq: u16,
	pub n: u8,
	/// XOR of the group's packet lengths.
	pub len_xor: u16,
	/// XOR of the group's packets, each zero-padded to the longest.
	pub xor: &'a [u8],
}

/// Build one parity session-payload over `pkts` (consecutive video datagrams starting at
/// `base_seq`). `None` for an empty group or one with an oversized packet.
pub fn parity_frame(base_seq: u16, pkts: &[&[u8]]) -> Option<Vec<u8>> {
	if pkts.is_empty() || pkts.len() > u8::MAX as usize {
		return None;
	}
	let max = pkts.iter().map(|p| p.len()).max()?;
	if max == 0 || max > FEC_MAX_PACKET {
		return None;
	}
	let mut out = Vec::with_capacity(6 + max);
	out.push(TAG_FEC);
	out.extend_from_slice(&base_seq.to_be_bytes());
	out.push(pkts.len() as u8);
	let len_xor = pkts.iter().fold(0u16, |acc, p| acc ^ p.len() as u16);
	out.extend_from_slice(&len_xor.to_be_bytes());
	let start = out.len();
	out.resize(start + max, 0);
	for p in pkts {
		for (dst, src) in out[start..].iter_mut().zip(p.iter()) {
			*dst ^= *src;
		}
	}
	Some(out)
}

/// Parse a parity datagram's body (the bytes after `TAG_FEC`, as `parse` returns them).
pub fn parse_parity(body: &[u8]) -> Option<Parity<'_>> {
	if body.len() < 6 {
		return None;
	}
	let base_seq = u16::from_be_bytes([body[0], body[1]]);
	let n = body[2];
	let len_xor = u16::from_be_bytes([body[3], body[4]]);
	if n == 0 {
		return None;
	}
	Some(Parity { base_seq, n, len_xor, xor: &body[5..] })
}

/// Rebuild the one missing packet of `parity`'s group. `present` yields the group's
/// packets we do have, keyed by sequence number (order irrelevant; extras ignored).
/// Returns `(seq, packet)` when exactly one packet of the group is absent, `None` when
/// the group is complete or more than one is missing (XOR can't repair that).
pub fn recover<'a>(parity: &Parity<'_>, present: impl Iterator<Item = (u16, &'a [u8])>) -> Option<(u16, Vec<u8>)> {
	let n = parity.n as usize;
	let mut have = vec![false; n];
	let mut buf = parity.xor.to_vec();
	let mut len = parity.len_xor;
	for (seq, pkt) in present {
		let idx = seq.wrapping_sub(parity.base_seq) as usize;
		if idx >= n || have[idx] {
			continue;
		}
		have[idx] = true;
		len ^= pkt.len() as u16;
		for (dst, src) in buf.iter_mut().zip(pkt.iter()) {
			*dst ^= *src;
		}
	}
	let missing: Vec<usize> = (0..n).filter(|&i| !have[i]).collect();
	if missing.len() != 1 {
		return None;
	}
	let len = len as usize;
	if len == 0 || len > buf.len() {
		return None;
	}
	buf.truncate(len);
	Some((parity.base_seq.wrapping_add(missing[0] as u16), buf))
}

/// Host side: groups outgoing video datagrams and emits a parity frame every `n`.
/// `n == 0` disables FEC (the group is dropped). A sequence gap at the intake (the encoder
/// restarted) restarts the group.
#[derive(Debug, Default)]
pub struct FecEncoder {
	n: u8,
	group: Vec<Vec<u8>>,
	base: u16,
	next: Option<u16>,
}

impl FecEncoder {
	pub fn new(n: u8) -> Self {
		Self { n, ..Default::default() }
	}

	pub fn n(&self) -> u8 {
		self.n
	}

	/// Change the group size live (takes effect at the next group boundary).
	pub fn set_n(&mut self, n: u8) {
		if n != self.n {
			self.n = n;
			self.group.clear();
			self.next = None;
		}
	}

	/// One outgoing video datagram (after it was sent). Returns the parity session-payload
	/// to send when this packet completes a group.
	pub fn push(&mut self, seq: u16, rtp: &[u8]) -> Option<Vec<u8>> {
		if self.n == 0 || rtp.is_empty() || rtp.len() > FEC_MAX_PACKET {
			return None;
		}
		if self.next != Some(seq) {
			// First packet, or a gap (encoder restart / a packet the intake never saw).
			self.group.clear();
			self.base = seq;
		}
		if self.group.is_empty() {
			self.base = seq;
		}
		self.next = Some(seq.wrapping_add(1));
		self.group.push(rtp.to_vec());
		if self.group.len() >= self.n as usize {
			let refs: Vec<&[u8]> = self.group.iter().map(|v| v.as_slice()).collect();
			let out = parity_frame(self.base, &refs);
			self.group.clear();
			return out;
		}
		None
	}
}

/// Client side: keeps the recent video datagrams by sequence and repairs from parity.
#[derive(Debug)]
pub struct FecDecoder {
	ring: std::collections::VecDeque<(u16, Vec<u8>)>,
	cap: usize,
	/// Group size seen in the latest parity (0 = none yet) — the controller deducts the
	/// parity share from the encoder rate.
	last_n: u8,
}

impl Default for FecDecoder {
	fn default() -> Self {
		Self::new(512)
	}
}

impl FecDecoder {
	pub fn new(cap: usize) -> Self {
		Self { ring: std::collections::VecDeque::with_capacity(cap.min(4096)), cap: cap.max(8), last_n: 0 }
	}

	pub fn last_n(&self) -> u8 {
		self.last_n
	}

	/// Remember a received (or already repaired) video datagram.
	pub fn on_packet(&mut self, seq: u16, rtp: &[u8]) {
		if self.ring.len() >= self.cap {
			self.ring.pop_front();
		}
		self.ring.push_back((seq, rtp.to_vec()));
	}

	/// A parity datagram arrived: rebuild the group's single missing packet, if any. The
	/// rebuilt packet is also remembered so a later parity over an overlapping range sees it.
	pub fn on_parity(&mut self, body: &[u8]) -> Option<(u16, Vec<u8>)> {
		let p = parse_parity(body)?;
		self.last_n = p.n;
		let n = p.n;
		let base = p.base_seq;
		let in_group = |seq: u16| seq.wrapping_sub(base) < n as u16;
		let (seq, pkt) = recover(&p, self.ring.iter().filter(|(s, _)| in_group(*s)).map(|(s, v)| (*s, v.as_slice())))?;
		self.on_packet(seq, &pkt);
		Some((seq, pkt))
	}

	/// The stream restarted (new sequence base): forget the old packets.
	pub fn reset(&mut self) {
		self.ring.clear();
	}
}

#[cfg(test)]
mod fec_tests {
	use super::*;

	fn rtp(seq: u16, len: usize) -> Vec<u8> {
		let mut v = vec![0x80u8, 96, (seq >> 8) as u8, seq as u8];
		v.extend((0..len.saturating_sub(4)).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seq as u8)));
		v
	}

	#[test]
	fn parity_frame_parses_as_media_and_rebuilds_the_missing_packet() {
		let pkts: Vec<Vec<u8>> = (0..8u16).map(|s| rtp(100 + s, 900 + (s as usize % 3) * 50)).collect();
		let refs: Vec<&[u8]> = pkts.iter().map(|v| v.as_slice()).collect();
		let frame = parity_frame(100, &refs).unwrap();
		let (tag, body) = parse(&frame).expect("parity is a media frame");
		assert_eq!(tag, TAG_FEC);
		let p = parse_parity(body).unwrap();
		assert_eq!((p.base_seq, p.n), (100, 8));
		assert_eq!(p.xor.len(), 1000);
		// Lose packet 103 (index 3): rebuild it from the other seven.
		let present = pkts.iter().enumerate().filter(|(i, _)| *i != 3).map(|(i, v)| (100 + i as u16, v.as_slice()));
		let (seq, rebuilt) = recover(&p, present).expect("one missing → repairable");
		assert_eq!(seq, 103);
		assert_eq!(rebuilt, pkts[3]);
		// Two missing → not repairable; none missing → nothing to do.
		let present2 = pkts.iter().enumerate().filter(|(i, _)| *i != 3 && *i != 5).map(|(i, v)| (100 + i as u16, v.as_slice()));
		assert!(recover(&p, present2).is_none());
		let all = pkts.iter().enumerate().map(|(i, v)| (100 + i as u16, v.as_slice()));
		assert!(recover(&p, all).is_none());
	}

	#[test]
	fn encoder_emits_one_parity_per_group_and_restarts_on_gaps() {
		let mut e = FecEncoder::new(4);
		let mut parities = 0;
		for s in 0..12u16 {
			if e.push(s, &rtp(s, 500)).is_some() {
				parities += 1;
			}
		}
		assert_eq!(parities, 3);
		// A gap restarts the group: 20,21,22 then 30 → no parity until 30..33 complete.
		let mut e = FecEncoder::new(4);
		for s in [20u16, 21, 22] {
			assert!(e.push(s, &rtp(s, 500)).is_none());
		}
		for s in [30u16, 31, 32] {
			assert!(e.push(s, &rtp(s, 500)).is_none());
		}
		let f = e.push(33, &rtp(33, 500)).expect("group 30..33 complete");
		assert_eq!(parse_parity(parse(&f).unwrap().1).unwrap().base_seq, 30);
		// n = 0 → off; set_n live.
		e.set_n(0);
		assert!(e.push(34, &rtp(34, 500)).is_none());
	}

	#[test]
	fn decoder_repairs_from_its_ring_across_wraparound() {
		let mut e = FecEncoder::new(5);
		let mut d = FecDecoder::new(64);
		let mut repaired = Vec::new();
		let start = 65533u16;
		for i in 0..10u16 {
			let seq = start.wrapping_add(i);
			let pkt = rtp(seq, 700);
			let parity = e.push(seq, &pkt);
			if seq != 1 {
				// Packet seq 1 (index 4 of the first group, across the wrap) is lost.
				d.on_packet(seq, &pkt);
			}
			if let Some(f) = parity {
				let (tag, body) = parse(&f).unwrap();
				assert_eq!(tag, TAG_FEC);
				if let Some((s, p)) = d.on_parity(body) {
					repaired.push((s, p));
				}
			}
		}
		assert_eq!(repaired.len(), 1);
		assert_eq!(repaired[0].0, 1);
		assert_eq!(repaired[0].1, rtp(1, 700));
		assert_eq!(d.last_n(), 5);
	}

	#[test]
	fn junk_parity_is_rejected() {
		assert!(parse_parity(&[1, 2, 3]).is_none());
		assert!(parse_parity(&[0, 1, 0, 0, 0, 0]).is_none(), "n = 0");
		assert!(parity_frame(0, &[]).is_none());
		let big = vec![0u8; FEC_MAX_PACKET + 1];
		assert!(parity_frame(0, &[&big]).is_none());
	}
}
