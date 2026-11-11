//! Pad microphone over the session: when it is on, and how its audio is framed.
//!
//! - Host: the virtual pad emits `Ds5HostEvent::MicOpen` when a game opens or closes its
//!   mic. [`MicAnnouncer`] turns that (and the client's `Ds5Features::mic`) into
//!   `DataMsg::Ds5Mic` sends: on every change, and every [`MIC_REFRESH`] while on,
//!   because session datagrams can drop.
//! - Client: [`MicGate`] holds the host's last word; without a refresh for
//!   [`MIC_TIMEOUT`] it reads as off, so a lost "off" (or a dead host) can't leave a BT
//!   pad streaming its mic (which halves its input rate).
//! - Audio: the pad mic travels as Opus (48 kHz mono, 10 ms) in `Ds5Media::Mic`. A BT pad
//!   produces those packets itself; for a USB pad the client encodes [`MicFramer`]
//!   blocks. The host decodes to mono and the virtual pad takes interleaved stereo
//!   ([`mono_to_stereo`]).

use std::time::{Duration, Instant};

/// The host repeats `Ds5Mic { on: true }` this often while the mic is open.
pub const MIC_REFRESH: Duration = Duration::from_secs(1);
/// The client treats the mic as closed after this long without a `Ds5Mic { on: true }`.
pub const MIC_TIMEOUT: Duration = Duration::from_secs(3);
/// Mono samples per mic frame (10 ms at 48 kHz).
pub const MIC_FRAME: usize = 480;
/// Mic sample rate.
pub const MIC_RATE: u32 = 48_000;

/// Host side: when to send `Ds5Mic`.
#[derive(Debug, Default)]
pub struct MicAnnouncer {
	open: bool,
	allowed: bool,
	/// The last value sent and when.
	sent: Option<(bool, Instant)>,
}

impl MicAnnouncer {
	pub fn new(allowed: bool) -> Self {
		Self {
			allowed,
			..Default::default()
		}
	}

	/// The game opened or closed the virtual mic.
	pub fn set_open(&mut self, open: bool) {
		self.open = open;
	}

	/// The client's `Ds5Features::mic` (its user may turn the pad mic off).
	pub fn set_allowed(&mut self, allowed: bool) {
		self.allowed = allowed;
	}

	/// Whether the client should stream its mic now.
	pub fn wanted(&self) -> bool {
		self.open && self.allowed
	}

	/// The `on` value to send at `now`, if one is due: on a change, and every
	/// [`MIC_REFRESH`] while on. Nothing is sent before the first open.
	pub fn poll(&mut self, now: Instant) -> Option<bool> {
		let want = self.wanted();
		let due = match self.sent {
			None => want,
			Some((last, at)) => last != want || (want && now.duration_since(at) >= MIC_REFRESH),
		};
		if due {
			self.sent = Some((want, now));
			Some(want)
		} else {
			None
		}
	}
}

/// Client side: whether the host currently wants the pad's mic.
#[derive(Debug, Default, Clone, Copy)]
pub struct MicGate {
	on_until: Option<Instant>,
}

impl MicGate {
	/// A `Ds5Mic` from the host.
	pub fn on_msg(&mut self, on: bool, now: Instant) {
		self.on_until = on.then(|| now + MIC_TIMEOUT);
	}

	pub fn is_on(&self, now: Instant) -> bool {
		self.on_until.is_some_and(|t| now < t)
	}
}

/// Collects captured mono samples into [`MIC_FRAME`]-sample blocks for the encoder.
#[derive(Debug, Default)]
pub struct MicFramer {
	pending: Vec<i16>,
}

impl MicFramer {
	/// Feed mono samples; every complete 10 ms block goes to `out`.
	pub fn push(&mut self, mono: &[i16], out: &mut Vec<[i16; MIC_FRAME]>) {
		self.pending.extend_from_slice(mono);
		while self.pending.len() >= MIC_FRAME {
			let mut b = [0i16; MIC_FRAME];
			b.copy_from_slice(&self.pending[..MIC_FRAME]);
			self.pending.drain(..MIC_FRAME);
			out.push(b);
		}
	}

	pub fn clear(&mut self) {
		self.pending.clear();
	}
}

/// Mono → interleaved stereo (the virtual pad's mic is 2 channels).
pub fn mono_to_stereo(mono: &[i16]) -> Vec<i16> {
	mono.iter().flat_map(|&v| [v, v]).collect()
}

/// Sequence numbers of a mic stream, and the receiver's gap count.
#[derive(Debug, Default)]
pub struct MicSeq {
	next: u16,
	last_rx: Option<u16>,
	/// Packets the receiver saw missing (lost or reordered away).
	pub lost: u64,
}

impl MicSeq {
	/// Sender: the next sequence number.
	pub fn next(&mut self) -> u16 {
		let s = self.next;
		self.next = self.next.wrapping_add(1);
		s
	}

	/// Receiver: account for `seq`. Returns false for a duplicate or late packet (drop it:
	/// the virtual mic is already past it).
	pub fn accept(&mut self, seq: u16) -> bool {
		match self.last_rx {
			Some(last) => {
				let fwd = seq.wrapping_sub(last);
				if fwd == 0 || fwd >= 0x8000 {
					return false;
				}
				self.lost += (fwd - 1) as u64;
			}
			None => {}
		}
		self.last_rx = Some(seq);
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn announcer_sends_changes_and_refreshes_while_on() {
		let t0 = Instant::now();
		let mut a = MicAnnouncer::new(true);
		assert_eq!(a.poll(t0), None, "nothing before the first open");
		a.set_open(true);
		assert_eq!(a.poll(t0), Some(true));
		assert_eq!(a.poll(t0 + Duration::from_millis(500)), None);
		assert_eq!(a.poll(t0 + MIC_REFRESH), Some(true), "refresh");
		a.set_open(false);
		let t1 = t0 + MIC_REFRESH + Duration::from_millis(10);
		assert_eq!(a.poll(t1), Some(false));
		assert_eq!(a.poll(t1 + MIC_REFRESH * 5), None, "off is not refreshed");
	}

	#[test]
	fn announcer_honours_the_client_feature() {
		let t0 = Instant::now();
		let mut a = MicAnnouncer::new(false);
		a.set_open(true);
		assert_eq!(a.poll(t0), None, "client turned the mic off");
		a.set_allowed(true);
		assert_eq!(a.poll(t0), Some(true));
		a.set_allowed(false);
		assert_eq!(a.poll(t0), Some(false));
	}

	#[test]
	fn gate_times_out_without_refresh() {
		let t0 = Instant::now();
		let mut g = MicGate::default();
		assert!(!g.is_on(t0));
		g.on_msg(true, t0);
		assert!(g.is_on(t0 + MIC_TIMEOUT - Duration::from_millis(1)));
		assert!(
			!g.is_on(t0 + MIC_TIMEOUT),
			"a lost off / dead host turns it off"
		);
		g.on_msg(true, t0 + MIC_REFRESH);
		assert!(g.is_on(t0 + MIC_TIMEOUT));
		g.on_msg(false, t0 + MIC_REFRESH * 2);
		assert!(!g.is_on(t0 + MIC_REFRESH * 2));
	}

	#[test]
	fn framer_makes_10ms_blocks() {
		let mut f = MicFramer::default();
		let mut out = Vec::new();
		let s: Vec<i16> = (0..1000).map(|i| i as i16).collect();
		f.push(&s[..300], &mut out);
		assert!(out.is_empty());
		f.push(&s[300..], &mut out);
		assert_eq!(out.len(), 2);
		assert_eq!(out[0][0], 0);
		assert_eq!(out[1][0], 480);
		assert_eq!(out[1][479], 959);
		f.push(&s[..440], &mut out);
		assert_eq!(out.len(), 3, "40 left over + 440 = one more block");
		assert_eq!(out[2][0], 960);
	}

	#[test]
	fn stereo_and_seq() {
		assert_eq!(mono_to_stereo(&[1, -2]), vec![1, 1, -2, -2]);
		let mut tx = MicSeq::default();
		let mut rx = MicSeq::default();
		let a = tx.next();
		let b = tx.next();
		let _lost = tx.next();
		let d = tx.next();
		assert!(rx.accept(a));
		assert!(rx.accept(b));
		assert!(!rx.accept(b), "duplicate");
		assert!(rx.accept(d));
		assert_eq!(rx.lost, 1);
		assert!(!rx.accept(a), "late");
		let mut w = MicSeq {
			next: 0xFFFF,
			..Default::default()
		};
		let x = w.next();
		let y = w.next();
		let mut r = MicSeq::default();
		assert!(r.accept(x) && r.accept(y), "wraps");
		assert_eq!(r.lost, 0);
	}
}
