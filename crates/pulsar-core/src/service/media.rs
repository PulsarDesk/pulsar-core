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
		Some((&tag @ (TAG_VIDEO | TAG_AUDIO | TAG_FEC | TAG_FEC_RS), rest)) if !rest.is_empty() => Some((tag, rest)),
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

// ── Reed-Solomon FEC (v2): per-frame parity, several losses per frame ────────────────────
//
// The XOR parity above repairs ONE loss per group. The gold standard of game streaming
// (Sunshine/Moonlight, Parsec) is Reed-Solomon per video frame: the frame's `k` packets are
// the data shards, `m = ceil(k × ratio)` parity shards ride behind them, and the client
// rebuilds ANY `≤ m` lost packets of that frame — still with zero round-trips. The ratio
// follows the measured loss (`adapt::fec_policy::parity_ratio`, 10–30 %), off on a clean
// path. A separate tag so old clients (XOR only) ignore it and old hosts never see it.
//
// Framing: `[TAG_FEC_RS][base_seq u16][k u8][m u8][idx u8][shard_len u16][shard…]` — one
// parity shard per datagram. Every DATA shard is `[len u16][rtp…][zero pad]` so a rebuilt
// shard carries its own length; parity shards are RS(k, m) over those.

/// Tag byte: a Reed-Solomon parity shard over one video frame's packets follows.
pub const TAG_FEC_RS: u8 = 0x04;
/// Feature id: the host can send Reed-Solomon parity (client asks with `StreamReq::fec_rs`).
pub const FEAT_FEC_RS: &str = "fec_rs";
/// Most data shards per RS block (a big keyframe is split into several blocks).
pub const RS_MAX_DATA: usize = 200;
const RS_HDR: usize = 7;

use reed_solomon_erasure::galois_8::ReedSolomon;

/// One parsed parity datagram (borrowed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RsParity<'a> {
	pub base_seq: u16,
	pub k: u8,
	pub m: u8,
	pub idx: u8,
	pub shard_len: u16,
	pub shard: &'a [u8],
}

/// Parse the body after `TAG_FEC_RS` (as `parse` returns it).
pub fn parse_rs_parity(body: &[u8]) -> Option<RsParity<'_>> {
	if body.len() < RS_HDR {
		return None;
	}
	let base_seq = u16::from_be_bytes([body[0], body[1]]);
	let (k, m, idx) = (body[2], body[3], body[4]);
	let shard_len = u16::from_be_bytes([body[5], body[6]]) as usize;
	if k == 0 || m == 0 || idx >= m || shard_len < 2 || body.len() < RS_HDR + shard_len {
		return None;
	}
	Some(RsParity { base_seq, k, m, idx, shard_len: shard_len as u16, shard: &body[RS_HDR..RS_HDR + shard_len] })
}

/// A length-prefixed, zero-padded data shard for `pkt`.
fn rs_data_shard(pkt: &[u8], shard_len: usize) -> Vec<u8> {
	let mut v = vec![0u8; shard_len];
	v[0..2].copy_from_slice(&(pkt.len() as u16).to_be_bytes());
	v[2..2 + pkt.len()].copy_from_slice(pkt);
	v
}

/// Build the parity datagrams for one block of consecutive packets (`base_seq` = the first
/// one's sequence number) at `ratio` (parity/data). Empty for an empty block or `ratio ≤ 0`.
pub fn rs_parity_frames(base_seq: u16, pkts: &[&[u8]], ratio: f32) -> Vec<Vec<u8>> {
	let k = pkts.len();
	if k == 0 || k > RS_MAX_DATA || ratio <= 0.0 {
		return Vec::new();
	}
	let max = pkts.iter().map(|p| p.len()).max().unwrap_or(0);
	if max == 0 || max > FEC_MAX_PACKET {
		return Vec::new();
	}
	let m = ((k as f32 * ratio).ceil() as usize).clamp(1, 255 - k);
	let shard_len = max + 2;
	let mut shards: Vec<Vec<u8>> = pkts.iter().map(|p| rs_data_shard(p, shard_len)).collect();
	shards.extend((0..m).map(|_| vec![0u8; shard_len]));
	let Ok(rs) = ReedSolomon::new(k, m) else { return Vec::new() };
	if rs.encode(&mut shards).is_err() {
		return Vec::new();
	}
	shards[k..]
		.iter()
		.enumerate()
		.map(|(j, parity)| {
			let mut v = Vec::with_capacity(1 + RS_HDR + shard_len);
			v.push(TAG_FEC_RS);
			v.extend_from_slice(&base_seq.to_be_bytes());
			v.push(k as u8);
			v.push(m as u8);
			v.push(j as u8);
			v.extend_from_slice(&(shard_len as u16).to_be_bytes());
			v.extend_from_slice(parity);
			v
		})
		.collect()
}

/// Host side: groups the outgoing video datagrams by FRAME (RTP timestamp / marker) and
/// emits the block's parity when the frame completes. `ratio == 0` disables FEC.
#[derive(Debug, Default)]
pub struct RsFecEncoder {
	ratio: f32,
	group: Vec<Vec<u8>>,
	base: u16,
	next: Option<u16>,
	cur_ts: Option<u32>,
}

impl RsFecEncoder {
	pub fn new(ratio: f32) -> Self {
		Self { ratio: ratio.max(0.0), ..Default::default() }
	}

	pub fn ratio(&self) -> f32 {
		self.ratio
	}

	/// Change the parity ratio live (applies from the next frame).
	pub fn set_ratio(&mut self, ratio: f32) {
		self.ratio = ratio.max(0.0);
	}

	fn flush(&mut self) -> Vec<Vec<u8>> {
		if self.group.is_empty() || self.ratio <= 0.0 {
			self.group.clear();
			return Vec::new();
		}
		let mut out = Vec::new();
		let mut base = self.base;
		let group = std::mem::take(&mut self.group);
		for chunk in group.chunks(RS_MAX_DATA) {
			let refs: Vec<&[u8]> = chunk.iter().map(|v| v.as_slice()).collect();
			out.extend(rs_parity_frames(base, &refs, self.ratio));
			base = base.wrapping_add(chunk.len() as u16);
		}
		out
	}

	/// One outgoing video datagram (after it was sent). Returns the parity datagrams to send
	/// (usually empty; the frame's parity when this packet closes the frame).
	pub fn push(&mut self, seq: u16, rtp: &[u8]) -> Vec<Vec<u8>> {
		if self.ratio <= 0.0 || rtp.len() < 12 || rtp.len() > FEC_MAX_PACKET {
			self.group.clear();
			self.next = Some(seq.wrapping_add(1));
			return Vec::new();
		}
		let ts = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
		let marker = rtp[1] & 0x80 != 0;
		let mut out = Vec::new();
		if self.next != Some(seq) {
			// A gap at the intake (encoder restart): the open frame is unrepairable — drop.
			self.group.clear();
		} else if self.cur_ts != Some(ts) && !self.group.is_empty() {
			// The previous frame never carried a marker: close it now.
			out.extend(self.flush());
		}
		if self.group.is_empty() {
			self.base = seq;
		}
		self.cur_ts = Some(ts);
		self.next = Some(seq.wrapping_add(1));
		self.group.push(rtp.to_vec());
		if marker {
			out.extend(self.flush());
		}
		out
	}
}

#[derive(Debug)]
struct RsBlock {
	base: u16,
	k: u8,
	m: u8,
	shard_len: usize,
	parity: Vec<Option<Vec<u8>>>,
	age: u32,
}

/// Client side: remembers the recent video datagrams and rebuilds a frame's missing ones
/// once enough parity shards for it have arrived.
#[derive(Debug)]
pub struct RsFecDecoder {
	ring: std::collections::VecDeque<(u16, Vec<u8>)>,
	cap: usize,
	blocks: Vec<RsBlock>,
	last_ratio: f32,
	tick: u32,
}

impl Default for RsFecDecoder {
	fn default() -> Self {
		Self::new(1024)
	}
}

impl RsFecDecoder {
	pub fn new(cap: usize) -> Self {
		Self { ring: std::collections::VecDeque::with_capacity(cap.min(4096)), cap: cap.max(16), blocks: Vec::new(), last_ratio: 0.0, tick: 0 }
	}

	/// Parity overhead (`m/k`) of the most recent block — what the controller deducts.
	pub fn last_ratio(&self) -> f32 {
		self.last_ratio
	}

	pub fn on_packet(&mut self, seq: u16, rtp: &[u8]) {
		if self.ring.len() >= self.cap {
			self.ring.pop_front();
		}
		self.ring.push_back((seq, rtp.to_vec()));
	}

	fn find(&self, seq: u16) -> Option<&[u8]> {
		self.ring.iter().rev().find(|(s, _)| *s == seq).map(|(_, p)| p.as_slice())
	}

	/// A parity datagram arrived: rebuild the block's missing packets if it is now
	/// repairable. Rebuilt packets are also remembered.
	pub fn on_parity(&mut self, body: &[u8]) -> Vec<(u16, Vec<u8>)> {
		let Some(p) = parse_rs_parity(body) else { return Vec::new() };
		self.tick += 1;
		self.last_ratio = p.m as f32 / p.k as f32;
		let pos = match self.blocks.iter().position(|b| b.base == p.base_seq && b.k == p.k && b.m == p.m) {
			Some(i) => i,
			None => {
				if self.blocks.len() >= 32 {
					self.blocks.remove(0);
				}
				self.blocks.push(RsBlock {
					base: p.base_seq,
					k: p.k,
					m: p.m,
					shard_len: p.shard_len as usize,
					parity: vec![None; p.m as usize],
					age: self.tick,
				});
				self.blocks.len() - 1
			}
		};
		{
			let b = &mut self.blocks[pos];
			if b.shard_len != p.shard_len as usize {
				return Vec::new();
			}
			b.parity[p.idx as usize] = Some(p.shard.to_vec());
			b.age = self.tick;
		}
		let (k, m, base, shard_len) = {
			let b = &self.blocks[pos];
			(b.k as usize, b.m as usize, b.base, b.shard_len)
		};
		// Data shards we have.
		let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
		let mut missing = Vec::new();
		for i in 0..k {
			let seq = base.wrapping_add(i as u16);
			match self.find(seq) {
				Some(pkt) if pkt.len() + 2 <= shard_len => shards.push(Some(rs_data_shard(pkt, shard_len))),
				_ => {
					missing.push(i);
					shards.push(None);
				}
			}
		}
		if missing.is_empty() {
			self.blocks.remove(pos);
			return Vec::new();
		}
		let parity_present = self.blocks[pos].parity.iter().filter(|s| s.is_some()).count();
		if missing.len() > parity_present {
			return Vec::new(); // wait for more parity (or give up when the block ages out)
		}
		shards.extend(self.blocks[pos].parity.iter().cloned());
		let Ok(rs) = ReedSolomon::new(k, m) else {
			self.blocks.remove(pos);
			return Vec::new();
		};
		if rs.reconstruct_data(&mut shards).is_err() {
			self.blocks.remove(pos);
			return Vec::new();
		}
		let mut out = Vec::new();
		for i in missing {
			if let Some(shard) = &shards[i] {
				let len = u16::from_be_bytes([shard[0], shard[1]]) as usize;
				if len >= 12 && len + 2 <= shard_len {
					let pkt = shard[2..2 + len].to_vec();
					let seq = base.wrapping_add(i as u16);
					self.on_packet(seq, &pkt);
					out.push((seq, pkt));
				}
			}
		}
		self.blocks.remove(pos);
		out
	}

	/// The stream restarted (new sequence base): forget everything.
	pub fn reset(&mut self) {
		self.ring.clear();
		self.blocks.clear();
	}
}

#[cfg(test)]
mod rs_fec_tests {
	use super::*;

	fn rtp(seq: u16, ts: u32, marker: bool, len: usize) -> Vec<u8> {
		let mut v = vec![0x80u8, if marker { 0x80 | 96 } else { 96 }, (seq >> 8) as u8, seq as u8];
		v.extend_from_slice(&ts.to_be_bytes());
		v.extend_from_slice(&[0, 0, 0, 1]);
		v.extend((0..len.saturating_sub(12)).map(|i| (i as u8).wrapping_mul(29).wrapping_add(seq as u8)));
		v
	}

	/// One 10-packet frame at ratio 0.25 → 3 parity; lose 3 packets → all rebuilt.
	#[test]
	fn rebuilds_several_losses_of_one_frame() {
		let mut enc = RsFecEncoder::new(0.25);
		let mut dec = RsFecDecoder::default();
		let pkts: Vec<Vec<u8>> = (0..10u16).map(|i| rtp(500 + i, 9000, i == 9, 900 + (i as usize % 4) * 60)).collect();
		let mut parity = Vec::new();
		for (i, p) in pkts.iter().enumerate() {
			let seq = 500 + i as u16;
			parity.extend(enc.push(seq, p));
			if ![2usize, 5, 7].contains(&i) {
				dec.on_packet(seq, p);
			}
		}
		assert_eq!(parity.len(), 3, "ceil(10 × 0.25) parity shards");
		let mut rebuilt = Vec::new();
		for f in &parity {
			let (tag, body) = parse(f).unwrap();
			assert_eq!(tag, TAG_FEC_RS);
			rebuilt.extend(dec.on_parity(body));
		}
		let mut got: Vec<u16> = rebuilt.iter().map(|(s, _)| *s).collect();
		got.sort();
		assert_eq!(got, vec![502, 505, 507]);
		for (s, p) in rebuilt {
			assert_eq!(p, pkts[(s - 500) as usize], "seq {s} byte-exact");
		}
		assert!((dec.last_ratio() - 0.3).abs() < 1e-6, "3/10 observed");
	}

	#[test]
	fn too_many_losses_wait_for_more_parity_then_give_up_cleanly() {
		let mut enc = RsFecEncoder::new(0.10); // 10 packets → 1 parity
		let mut dec = RsFecDecoder::default();
		let mut parity = Vec::new();
		for i in 0..10u16 {
			let p = rtp(i, 1, i == 9, 500);
			parity.extend(enc.push(i, &p));
			if i != 3 && i != 4 {
				dec.on_packet(i, &p);
			}
		}
		assert_eq!(parity.len(), 1);
		let (_, body) = parse(&parity[0]).unwrap();
		assert!(dec.on_parity(body).is_empty(), "2 lost, 1 parity: not repairable");
		// A complete frame's parity is a no-op.
		let mut enc2 = RsFecEncoder::new(0.10);
		let mut par2 = Vec::new();
		for i in 20..25u16 {
			let p = rtp(i, 2, i == 24, 300);
			par2.extend(enc2.push(i, &p));
			dec.on_packet(i, &p);
		}
		let (_, body) = parse(&par2[0]).unwrap();
		assert!(dec.on_parity(body).is_empty());
	}

	#[test]
	fn frames_close_on_marker_or_timestamp_change_and_gaps_restart() {
		let mut enc = RsFecEncoder::new(0.20);
		// Frame A without a marker (3 pkts), frame B starts → A's parity flushes.
		assert!(enc.push(1, &rtp(1, 100, false, 400)).is_empty());
		assert!(enc.push(2, &rtp(2, 100, false, 400)).is_empty());
		assert!(enc.push(3, &rtp(3, 100, false, 400)).is_empty());
		let out = enc.push(4, &rtp(4, 200, false, 400));
		assert_eq!(out.len(), 1, "frame A flushed on ts change");
		assert_eq!(parse_rs_parity(parse(&out[0]).unwrap().1).unwrap().base_seq, 1);
		// A gap drops the open frame B.
		let out = enc.push(9, &rtp(9, 200, true, 400));
		assert_eq!(out.len(), 1, "only the single packet 9 forms a block");
		assert_eq!(parse_rs_parity(parse(&out[0]).unwrap().1).unwrap().base_seq, 9);
		// ratio 0 → nothing.
		enc.set_ratio(0.0);
		assert!(enc.push(10, &rtp(10, 300, true, 400)).is_empty());
	}

	#[test]
	fn big_frames_split_into_blocks_and_wraparound_works() {
		let mut enc = RsFecEncoder::new(0.10);
		let mut dec = RsFecDecoder::new(2048);
		let start = 65500u16;
		let n = 250u16; // > RS_MAX_DATA → two blocks
		let mut parity = Vec::new();
		for i in 0..n {
			let seq = start.wrapping_add(i);
			let p = rtp(seq, 7, i == n - 1, 1100);
			parity.extend(enc.push(seq, &p));
			if i != 10 && i != 240 {
				dec.on_packet(seq, &p);
			}
		}
		assert!(parity.len() >= 2, "{}", parity.len());
		let mut rebuilt = Vec::new();
		for f in &parity {
			rebuilt.extend(dec.on_parity(parse(f).unwrap().1));
		}
		let mut got: Vec<u16> = rebuilt.iter().map(|(s, _)| *s).collect();
		got.sort();
		assert_eq!(got, vec![start.wrapping_add(240), start.wrapping_add(10)].into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>());
	}

	#[test]
	fn junk_is_rejected() {
		assert!(parse_rs_parity(&[0; 5]).is_none());
		assert!(parse_rs_parity(&[0, 1, 0, 1, 0, 0, 4]).is_none(), "k = 0");
		assert!(parse_rs_parity(&[0, 1, 3, 1, 1, 0, 4, 0, 0, 0, 0]).is_none(), "idx ≥ m");
		assert!(rs_parity_frames(0, &[], 0.2).is_empty());
		let mut d = RsFecDecoder::default();
		assert!(d.on_parity(&[1, 2, 3]).is_empty());
	}
}
