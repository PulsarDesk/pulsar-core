//! `Auto` mode path choice: a working hole-punch doesn't make the direct path the
//! FASTER one (a bad peering route can be ~500 ms while the relay answers in ~30 ms).
//! Once the direct path is proven, both ends time a few probes over each path and keep
//! the relay when it is clearly quicker.
//!
//! Probes reuse the existing wire messages, so nothing changes on the relay or in the
//! protocol: `Punch`/`PunchAck` with a `seq` in the [`PROBE_SEQ`] range, sent straight to
//! the peer (any peer acks those — the hole-punch arm already does) and tunnelled through
//! the relay inside `RelayData` (only peers with this code answer; without an answer the
//! direct path is kept, i.e. the old behaviour).
//!
//! Each side decides for its OWN sending direction (the host's choice is the one that
//! carries the video). A relay-tunnelled ack is unauthenticated, so a hostile relay could
//! only ever talk us INTO the relay path — which it already carries for rendezvous and
//! which only sees ciphertext.

use std::collections::HashMap;
use std::time::Instant;

use super::*;

/// Probe `seq`s start here — far above the 0..PUNCH_ATTEMPTS hole-punch burst.
pub(super) const PROBE_SEQ: u32 = 0x8000_0000;
/// Relay-path probes add this to their index (direct ones don't).
const RELAY_BIT: u32 = 0x1000;
/// Probes per path.
const PROBES: u32 = 5;
const PROBE_GAP: Duration = Duration::from_millis(40);
/// Give up waiting for answers after this long (from the first probe).
const PROBE_WINDOW: Duration = Duration::from_millis(900);
/// The relay must beat the direct path by this much AND by [`RELAY_RATIO`] to be chosen —
/// the relay costs bandwidth (and may carry a per-session rate cap), so near-ties stay direct.
const RELAY_MARGIN_MS: f32 = 20.0;
const RELAY_RATIO: f32 = 0.7;

/// One session's in-flight measurement.
#[derive(Default)]
pub(super) struct PathProbe {
	sent: HashMap<u32, Instant>,
	direct: Vec<f32>,
	relay: Vec<f32>,
}

impl PathProbe {
	fn record(&mut self, seq: u32) {
		if let Some(at) = self.sent.remove(&seq) {
			let ms = at.elapsed().as_secs_f32() * 1000.0;
			if seq - PROBE_SEQ >= RELAY_BIT {
				self.relay.push(ms);
			} else {
				self.direct.push(ms);
			}
		}
	}

	fn complete(&self) -> bool {
		self.direct.len() as u32 >= PROBES && self.relay.len() as u32 >= PROBES
	}
}

fn best(v: &[f32]) -> Option<f32> {
	v.iter().copied().reduce(f32::min)
}

/// Keep the relay instead of the proven direct path? Best (minimum) RTT per path: the
/// relay must be clearly faster; no relay answers (an older peer) → stay direct; a
/// direct path that never answered the probes while the relay did → relay.
pub(super) fn prefer_relay(direct: &[f32], relay: &[f32]) -> bool {
	match (best(direct), best(relay)) {
		(_, None) => false,
		(None, Some(_)) => true,
		(Some(d), Some(r)) => r + RELAY_MARGIN_MS < d && r < d * RELAY_RATIO,
	}
}

impl Node {
	/// Measure both paths of a session whose direct path was just proven and, if the relay
	/// is clearly faster, pin it (later punches no longer flip the session to `Direct`).
	/// Runs once per session; `Auto` mode only. Returns the transport to use.
	pub(super) async fn pick_faster_path(&self, session: SessionId) -> Transport {
		let (peer_addr, creds) = {
			let mut g = self.inner.lock().await;
			let creds = g.self_id.zip(g.token);
			let Some(s) = g.sessions.get_mut(&session) else {
				return Transport::Relay;
			};
			if self.mode != NetworkMode::Auto || !s.direct_ok || s.path_checked {
				return s.transport;
			}
			s.path_checked = true;
			let addr = s.peer_addr;
			g.probes.insert(session, PathProbe::default());
			(addr, creds)
		};
		let Some((id, token)) = creds else {
			self.inner.lock().await.probes.remove(&session);
			return Transport::Direct;
		};

		let start = Instant::now();
		for i in 0..PROBES {
			let (d_seq, r_seq) = (PROBE_SEQ + i, PROBE_SEQ + RELAY_BIT + i);
			{
				let mut g = self.inner.lock().await;
				if let Some(p) = g.probes.get_mut(&session) {
					p.sent.insert(d_seq, Instant::now());
					p.sent.insert(r_seq, Instant::now());
				}
			}
			let _ = self
				.sock
				.send_to(&encode(&PeerMsg::Punch { session, seq: d_seq }), peer_addr)
				.await;
			let _ = self
				.sock
				.send_to(
					&encode(&ClientMsg::RelayData {
						id,
						token,
						session,
						payload: encode(&PeerMsg::Punch { session, seq: r_seq }),
					}),
					self.relay,
				)
				.await;
			tokio::time::sleep(PROBE_GAP).await;
		}
		// Wait for the answers (or the window), checking a few times rather than per ack.
		while start.elapsed() < PROBE_WINDOW {
			if self.inner.lock().await.probes.get(&session).is_none_or(PathProbe::complete) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(15)).await;
		}

		let mut g = self.inner.lock().await;
		let probe = g.probes.remove(&session).unwrap_or_default();
		let relay = prefer_relay(&probe.direct, &probe.relay);
		tracing::info!(
			direct_ms = ?best(&probe.direct),
			relay_ms = ?best(&probe.relay),
			direct_n = probe.direct.len(),
			relay_n = probe.relay.len(),
			chosen = if relay { "relay" } else { "direct" },
			"auto path: compared direct vs relay latency"
		);
		let Some(s) = g.sessions.get_mut(&session) else {
			return Transport::Relay;
		};
		s.path_ms = Some((best(&probe.direct), best(&probe.relay)));
		if relay {
			s.relay_preferred = true;
			s.transport = Transport::Relay;
		}
		s.transport
	}

	/// A probe answer arrived (direct `PunchAck`, or one tunnelled through the relay).
	pub(super) async fn record_probe(&self, session: SessionId, seq: u32) {
		if seq < PROBE_SEQ {
			return;
		}
		if let Some(p) = self.inner.lock().await.probes.get_mut(&session) {
			p.record(seq);
		}
	}

	/// A relay-tunnelled `Punch` probe from the peer: answer through the relay so it can
	/// time that path. Only probe-range seqs of a session we know (never a data path change).
	pub(super) async fn answer_relay_probe(&self, session: SessionId, seq: u32) {
		if seq < PROBE_SEQ || self.mode == NetworkMode::RelayOnly {
			return;
		}
		let creds = {
			let g = self.inner.lock().await;
			if !g.sessions.contains_key(&session) {
				return;
			}
			g.self_id.zip(g.token)
		};
		let Some((id, token)) = creds else { return };
		let _ = self
			.sock
			.send_to(
				&encode(&ClientMsg::RelayData {
					id,
					token,
					session,
					payload: encode(&PeerMsg::PunchAck { session, seq }),
				}),
				self.relay,
			)
			.await;
	}

	/// A probe or probe answer came through the relay for this session: the relay path is alive
	/// (an input of the watchdog's fallback decision below). Unauthenticated like the probes
	/// themselves, so a hostile relay could only talk us INTO the relay path — see the module doc.
	pub(super) async fn relay_heard(&self, session: SessionId) {
		if let Some(s) = self.inner.lock().await.sessions.get(&session) {
			s.stamps.mark_relay(Instant::now());
		}
	}
}

// ---- Mid-session path watchdog ---------------------------------------------------------------
//
// The path choice above runs once. A direct path that later dies (a carrier NAT drops the
// mapping, a Wi-Fi <-> cellular switch, a middlebox that starts eating the flow) used to strand
// the session: `direct_ok` latched, every packet kept going to a dead address and the session
// ended on the app's silence timeout although the relay was still there. The watchdog keeps the
// choice live:
// - direct and silent (no packet from the peer's direct address) for [`PROBE_AFTER`]: probe it
//   (one `Punch`); still silent at [`RELAY_PROBE_AFTER`]: probe the relay path too;
// - direct silent for [`DEAD_AFTER`] while the relay answered DURING that silence: move the
//   session onto the relay (`transport` -> `Relay`, `direct_ok` cleared) without dropping it,
//   and punch on in the background so a recovered direct path is picked up again (after a
//   hold-down, so a path that keeps dying can't flap the session every few seconds);
// - a quiet session whose direct probe IS answered stays put — the peer is just idle.
// A healthy streaming session never reaches [`PROBE_AFTER`] (media and pings flow both ways),
// so a stable LAN/direct session sends nothing extra except the relay keepalive below.
//
// The receive stamps are written per datagram, so they are atomics shared through an `Arc`:
// the hot `deliver` path must not take the node lock a second time per packet.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Weak;

/// Direct-path silence before the watchdog starts probing it.
const PROBE_AFTER: Duration = Duration::from_millis(1500);
/// Direct-path silence before the relay path is probed too (it is the fallback candidate).
const RELAY_PROBE_AFTER: Duration = Duration::from_millis(2000);
/// Direct-path silence that counts as a dead path (when the relay is known to be alive).
/// Well under the client's 5 s session-silence teardown, so the fallback lands first.
const DEAD_AFTER: Duration = Duration::from_millis(3000);
/// A relay answer counts as "the relay is alive" for this long.
const RELAY_FRESH: Duration = Duration::from_millis(3000);
/// Probe cadence while the direct path is silent; slower once the silence is long (both paths
/// down, e.g. the network is gone — nothing to gain from probing hard).
const PROBE_EVERY: Duration = Duration::from_millis(500);
const PROBE_EVERY_LONG: Duration = Duration::from_secs(2);
const LONG_SILENCE: Duration = Duration::from_secs(10);
/// A direct session sends one tiny relay-tunnelled probe this often: the relay forgets a
/// session after 120 s without `RelayData` (its `SESSION_TTL`), and a session the relay forgot
/// can't fall back to it.
const RELAY_KEEPALIVE: Duration = Duration::from_secs(30);
/// After a fallback a re-proven direct path is not adopted for this long; doubled per further
/// fallback (capped at 16x) so a path that keeps dying settles on the relay instead of flapping.
const UPGRADE_HOLD: Duration = Duration::from_secs(10);
/// NAT rebinding: an authenticated packet from a new address is followed only once the current
/// address has been silent this long — while the old address still delivers, a packet from
/// elsewhere is a duplicate or a replay, not a move.
const REBIND_QUIET: Duration = Duration::from_millis(1000);
/// A rebinding candidate's path check must be answered within this long; a candidate that keeps
/// sending after that gets a fresh check (a lost one costs a retry, not the move).
const REBIND_CHALLENGE_TTL: Duration = Duration::from_millis(1000);
/// Authenticated data arriving THROUGH THE RELAY on a session still sending direct, with nothing
/// from the peer's direct address for this long, means the peer has fallen back (its watchdog
/// found our direct packets dead): follow it at once. Waiting for our own [`DEAD_AFTER`] put a
/// one-way-dead path's fallback at ~6 s, after the app's 5 s silence teardown on the other side.
/// A healthy direct peer sends something direct (pings, stats, input, media) every ~500 ms, so
/// 1 s is not a gap a working path shows.
const PEER_MOVED_AFTER: Duration = Duration::from_millis(1000);
const WATCH_TICK: Duration = Duration::from_millis(250);
/// Watchdog probe `seq`s: inside the probe range (the peer acks them like any probe and answers
/// the relay-tunnelled ones through the relay) but clear of `pick_faster_path`'s.
const WATCH_SEQ: u32 = PROBE_SEQ + 0x0010_0000;

/// Per-session receive stamps: the last packet from the peer's current direct address and the
/// last one through the relay. Microseconds since `base`, `0` = never.
pub(super) struct LinkStamps {
	base: Instant,
	direct: AtomicU64,
	relay: AtomicU64,
}

impl LinkStamps {
	pub(super) fn new() -> Arc<Self> {
		Arc::new(Self {
			base: Instant::now(),
			direct: AtomicU64::new(0),
			relay: AtomicU64::new(0),
		})
	}

	fn enc(&self, at: Instant) -> u64 {
		at.saturating_duration_since(self.base).as_micros() as u64 + 1
	}

	fn dec(&self, v: u64) -> Option<Instant> {
		(v != 0).then(|| self.base + Duration::from_micros(v - 1))
	}

	/// Monotonic (`fetch_max`): a writer that read the clock earlier never moves a stamp back.
	pub(super) fn mark_direct(&self, at: Instant) {
		self.direct.fetch_max(self.enc(at), Ordering::Relaxed);
	}

	pub(super) fn mark_relay(&self, at: Instant) {
		self.relay.fetch_max(self.enc(at), Ordering::Relaxed);
	}

	pub(super) fn direct(&self) -> Option<Instant> {
		self.dec(self.direct.load(Ordering::Relaxed))
	}

	pub(super) fn relay(&self) -> Option<Instant> {
		self.dec(self.relay.load(Ordering::Relaxed))
	}
}

/// The watchdog's per-session bookkeeping (in `SessionState`, under the node lock).
#[derive(Default)]
pub(super) struct PathWatch {
	last_probe: Option<Instant>,
	probe_n: u32,
	relay_keep: Option<Instant>,
	/// When the session last fell back from direct to the relay, and how often it has.
	pub(super) fell_back_at: Option<Instant>,
	fallbacks: u32,
	/// Bumped by each fallback: a background punch loop started for an earlier relay stint sees
	/// the change and exits, so one loop at most punches per session.
	pub(super) punch_gen: u32,
	/// A NAT-rebinding candidate waiting for its path check (see [`Node::maybe_rebind`]).
	rebind: Option<RebindCheck>,
}

/// The path check of a rebinding candidate: a `Punch` with an unguessable probe-range `seq` sent
/// to the new address; only its `PunchAck` FROM that address moves the session there.
#[derive(Clone, Copy, Debug)]
struct RebindCheck {
	addr: SocketAddr,
	seq: u32,
	sent: Instant,
}

#[cfg(test)]
impl PathWatch {
	/// The pending rebinding check: the address under check and the `seq` its answer must echo.
	pub(super) fn rebind_check(&self) -> Option<(SocketAddr, u32)> {
		self.rebind.map(|c| (c.addr, c.seq))
	}
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WatchAction {
	Idle,
	/// Send a direct probe (and a relay-tunnelled one too when `relay`).
	Probe { relay: bool },
	FallBack,
}

/// One watchdog decision for a session on its direct path. `direct` / `relay` are the last
/// receive stamps per path, `last_probe` the watchdog's last probe.
pub(super) fn assess(
	direct: Option<Instant>,
	relay: Option<Instant>,
	last_probe: Option<Instant>,
	now: Instant,
) -> WatchAction {
	// Stamped when the direct path is adopted, so `None` only for a path never proven.
	let Some(direct) = direct else {
		return WatchAction::Idle;
	};
	let silent = now.saturating_duration_since(direct);
	if silent < PROBE_AFTER {
		return WatchAction::Idle;
	}
	// The relay must have answered AFTER the direct path went quiet: an answer from before (a
	// keepalive) says nothing about now, and with both paths down (the network is gone) moving
	// to the relay would only add a hold-down once the direct path comes back.
	let relay_alive =
		relay.is_some_and(|r| r > direct && now.saturating_duration_since(r) < RELAY_FRESH);
	if silent >= DEAD_AFTER && relay_alive {
		return WatchAction::FallBack;
	}
	let every = if silent >= LONG_SILENCE {
		PROBE_EVERY_LONG
	} else {
		PROBE_EVERY
	};
	if last_probe.is_some_and(|p| now.saturating_duration_since(p) < every) {
		return WatchAction::Idle;
	}
	WatchAction::Probe {
		relay: silent >= RELAY_PROBE_AFTER,
	}
}

/// Is a session that fell back still inside its hold-down (a re-proven direct path is not
/// adopted yet)?
pub(super) fn upgrade_held(w: &PathWatch, now: Instant) -> bool {
	let hold = UPGRADE_HOLD * (1u32 << w.fallbacks.saturating_sub(1).min(4));
	w.fell_back_at
		.is_some_and(|t| now.saturating_duration_since(t) < hold)
}

/// May an authenticated packet from a new address move the session's direct address there?
/// Only once the current address has been quiet for [`REBIND_QUIET`] (see there).
pub(super) fn may_rebind(last_from_current: Option<Instant>, now: Instant) -> bool {
	last_from_current.is_none_or(|t| now.saturating_duration_since(t) >= REBIND_QUIET)
}

/// Has the peer moved onto the relay? Authenticated relay data on a session that sends direct
/// while its direct address has been quiet for [`PEER_MOVED_AFTER`] (see there).
pub(super) fn peer_moved(last_direct: Option<Instant>, now: Instant) -> bool {
	last_direct.is_some_and(|t| now.saturating_duration_since(t) >= PEER_MOVED_AFTER)
}

/// Move a session off its direct path onto the relay without dropping it. The direct address is
/// kept for the background re-punch the caller starts (`punch_in_background`); a re-proven path
/// is taken back after the hold-down ([`upgrade_held`]).
fn fall_back(s: &mut SessionState, now: Instant) {
	s.transport = Transport::Relay;
	s.direct_ok = false;
	let w = &mut s.watch;
	w.fell_back_at = Some(now);
	w.fallbacks = w.fallbacks.saturating_add(1);
	w.punch_gen = w.punch_gen.wrapping_add(1);
	w.last_probe = None;
	w.rebind = None;
}

impl Node {
	/// One watchdog pass over the sessions (see the section comment). `now` is a parameter so the
	/// tests can drive the clock.
	pub(super) async fn watch_tick(self: &Arc<Self>, now: Instant) {
		if self.mode != NetworkMode::Auto {
			return;
		}
		// (session, direct addr, direct probe seq, relay probe seq)
		let mut probes: Vec<(SessionId, SocketAddr, Option<u32>, Option<u32>)> = Vec::new();
		let mut fallen: Vec<(SessionId, SocketAddr, u128)> = Vec::new();
		let creds = {
			let mut g = self.inner.lock().await;
			let creds = g.self_id.zip(g.token);
			for (&id, s) in g.sessions.iter_mut() {
				// Only a relay-rendezvous session on its direct path has somewhere to fall back to
				// (a direct-IP session has no relay; a relay-preferred one is on it already).
				if !s.has_relay || s.transport != Transport::Direct || !s.direct_ok || s.relay_preferred
				{
					continue;
				}
				let w = &mut s.watch;
				let n = w.probe_n & 0xFFF;
				match assess(s.stamps.direct(), s.stamps.relay(), w.last_probe, now) {
					WatchAction::Idle => {}
					WatchAction::Probe { relay } => {
						w.last_probe = Some(now);
						w.probe_n = w.probe_n.wrapping_add(1);
						let relay_seq = (relay && creds.is_some()).then_some(WATCH_SEQ + RELAY_BIT + n);
						if relay_seq.is_some() {
							w.relay_keep = Some(now);
						}
						probes.push((id, s.peer_addr, Some(WATCH_SEQ + n), relay_seq));
						continue;
					}
					// Without relay credentials (not registered right now) the relay can't carry
					// the session — stay, and keep probing the direct path.
					WatchAction::FallBack if creds.is_some() => {
						let silent_ms = s
							.stamps
							.direct()
							.map_or(0, |d| now.saturating_duration_since(d).as_millis());
						fall_back(s, now);
						fallen.push((id, s.peer_addr, silent_ms));
						continue;
					}
					WatchAction::FallBack => {}
				}
				// Keep the relay's session entry alive while we're direct.
				match w.relay_keep {
					None => w.relay_keep = Some(now),
					Some(t) if creds.is_some() && now.saturating_duration_since(t) >= RELAY_KEEPALIVE => {
						w.relay_keep = Some(now);
						w.probe_n = w.probe_n.wrapping_add(1);
						probes.push((id, s.peer_addr, None, Some(WATCH_SEQ + RELAY_BIT + n)));
					}
					Some(_) => {}
				}
			}
			creds
		};
		for (session, addr, direct_seq, relay_seq) in probes {
			if let Some(seq) = direct_seq {
				let _ = self
					.sock
					.send_to(&encode(&PeerMsg::Punch { session, seq }), addr)
					.await;
			}
			if let (Some(seq), Some((id, token))) = (relay_seq, creds) {
				let _ = self
					.sock
					.send_to(
						&encode(&ClientMsg::RelayData {
							id,
							token,
							session,
							payload: encode(&PeerMsg::Punch { session, seq }),
						}),
						self.relay,
					)
					.await;
			}
		}
		for (session, addr, silent_ms) in fallen {
			tracing::info!(
				session,
				peer = %addr,
				silent_ms,
				"p2p: direct path went silent while the relay answers — the session moves onto the relay, punching on for a later upgrade"
			);
			self.punch_in_background(session, 0);
		}
	}

	/// NAT rebinding: an AUTHENTICATED packet (it decrypted under this session's key and passed the
	/// anti-replay window) arrived from an address other than the session's direct one, and the
	/// current address has gone quiet (see [`may_rebind`]). Never driven by unauthenticated
	/// traffic: a spoofed or reflected source can't produce such a packet.
	///
	/// It is not followed on the spot, though: the AEAD doesn't bind the source address, so an
	/// on-path attacker that withheld a few genuine packets (they stay fresh inside the
	/// anti-replay window) while dropping the real flow could replay one with a third party's
	/// source and turn the host's video into a flood at it. The new address must answer first,
	/// as in QUIC's path validation: a `Punch` with an unguessable `seq` goes there, and only its
	/// `PunchAck` from that address moves the session ([`Node::rebind_answered`]). Until then
	/// everything still goes to the old address. A real rebinding costs one RTT (the peer's NAT
	/// just mapped the new address, so the check gets through). Should the new address stop
	/// answering later, the watchdog moves the session onto the relay within [`DEAD_AFTER`].
	pub(super) async fn maybe_rebind(&self, session: SessionId, from: SocketAddr, now: Instant) {
		if self.mode == NetworkMode::RelayOnly {
			return;
		}
		let seq = {
			let mut g = self.inner.lock().await;
			let Some(s) = g.sessions.get_mut(&session) else {
				return;
			};
			if s.transport != Transport::Direct || from == s.peer_addr {
				return;
			}
			if !may_rebind(s.stamps.direct(), now) {
				return;
			}
			// One check per candidate in flight: media from the new address arrives per packet,
			// the check must not become a punch per packet.
			if s.watch.rebind.is_some_and(|c| {
				c.addr == from && now.saturating_duration_since(c.sent) < REBIND_CHALLENGE_TTL
			}) {
				return;
			}
			// 31 random bits in the probe range: the peer acks any punch, and the probe
			// bookkeeping ignores seqs it didn't send.
			let seq = PROBE_SEQ | (rand::random::<u32>() & !PROBE_SEQ);
			s.watch.rebind = Some(RebindCheck { addr: from, seq, sent: now });
			tracing::debug!(session, new = %from, "p2p: the peer may have a new address — checking it");
			seq
		};
		let _ = self
			.sock
			.send_to(&encode(&PeerMsg::Punch { session, seq }), from)
			.await;
	}

	/// A `PunchAck` that may answer a rebinding check (see [`Node::maybe_rebind`]): `true` when it
	/// did and the session now sends to `from`. Called with the node lock held.
	pub(super) fn rebind_answered(s: &mut SessionState, from: SocketAddr, seq: u32, now: Instant) -> bool {
		let Some(c) = s.watch.rebind else {
			return false;
		};
		if c.addr != from || c.seq != seq {
			return false;
		}
		s.watch.rebind = None;
		if s.transport != Transport::Direct || now.saturating_duration_since(c.sent) >= REBIND_CHALLENGE_TTL * 2 {
			return false;
		}
		tracing::info!(
			old = %s.peer_addr,
			new = %from,
			"p2p: the peer's address changed (NAT rebinding, new address answered) — sending to it"
		);
		s.peer_addr = from;
		s.stamps.mark_direct(now);
		true
	}

	/// The peer sent authenticated data through the relay while we still send direct and its
	/// direct address has gone quiet: it fell back — follow it now (see [`PEER_MOVED_AFTER`]).
	pub(super) async fn peer_moved_to_relay(self: &Arc<Self>, session: SessionId, now: Instant) {
		if self.mode != NetworkMode::Auto {
			return;
		}
		let addr = {
			let mut g = self.inner.lock().await;
			if g.self_id.zip(g.token).is_none() {
				return; // not registered: the relay can't carry our side
			}
			let Some(s) = g.sessions.get_mut(&session) else {
				return;
			};
			if !s.has_relay
				|| s.transport != Transport::Direct
				|| !s.direct_ok
				|| s.relay_preferred
				|| !peer_moved(s.stamps.direct(), now)
			{
				return;
			}
			fall_back(s, now);
			s.peer_addr
		};
		tracing::info!(
			session,
			peer = %addr,
			"p2p: the peer moved onto the relay while its direct address went quiet — following it, punching on for a later upgrade"
		);
		self.punch_in_background(session, 0);
	}
}

/// Run [`Node::watch_tick`] every [`WATCH_TICK`] until the node is dropped. `Auto` mode only (the
/// other modes have no direct/relay choice to revisit).
pub(super) async fn watch_loop(weak: Weak<Node>) {
	match weak.upgrade() {
		Some(n) if n.mode == NetworkMode::Auto => {}
		_ => return,
	}
	let mut tick = tokio::time::interval(WATCH_TICK);
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	loop {
		tick.tick().await;
		let Some(node) = weak.upgrade() else { return };
		node.watch_tick(Instant::now()).await;
	}
}

#[cfg(test)]
mod tests {
	use super::prefer_relay;
	use super::{assess, may_rebind, peer_moved, upgrade_held, LinkStamps, PathWatch, WatchAction};
	use std::time::{Duration, Instant};

	fn ms(v: u64) -> Duration {
		Duration::from_millis(v)
	}

	#[test]
	fn a_brief_gap_does_nothing_and_a_longer_one_only_probes() {
		let t0 = Instant::now();
		let direct = Some(t0);
		assert_eq!(assess(direct, None, None, t0 + ms(1000)), WatchAction::Idle, "1 s gap: no flap, no probe");
		assert_eq!(
			assess(direct, None, None, t0 + ms(1600)),
			WatchAction::Probe { relay: false },
			"the direct path is probed first"
		);
		assert_eq!(
			assess(direct, None, Some(t0 + ms(1600)), t0 + ms(1800)),
			WatchAction::Idle,
			"probes are paced"
		);
		assert_eq!(assess(direct, None, Some(t0 + ms(1600)), t0 + ms(2200)), WatchAction::Probe { relay: true });
	}

	#[test]
	fn a_dead_direct_path_falls_back_only_while_the_relay_answers() {
		let t0 = Instant::now();
		let direct = Some(t0);
		// No relay answer during the silence (both paths down): keep probing, don't move.
		assert_eq!(assess(direct, None, None, t0 + ms(3500)), WatchAction::Probe { relay: true });
		assert_eq!(
			assess(direct, Some(t0 - ms(200)), None, t0 + ms(3500)),
			WatchAction::Probe { relay: true },
			"a relay answer from before the silence (keepalive) proves nothing"
		);
		// The relay answered during the silence.
		assert_eq!(assess(direct, Some(t0 + ms(2300)), None, t0 + ms(3000)), WatchAction::FallBack);
		assert_eq!(
			assess(direct, Some(t0 + ms(2300)), None, t0 + ms(2900)),
			WatchAction::Probe { relay: true },
			"not before DEAD_AFTER"
		);
		// A relay answer gone stale again doesn't count either.
		assert_eq!(
			assess(direct, Some(t0 + ms(2300)), Some(t0 + ms(9000)), t0 + ms(9400)),
			WatchAction::Idle
		);
		// Direct came back (an answered probe / data): idle again.
		assert_eq!(assess(Some(t0 + ms(2900)), Some(t0 + ms(2300)), None, t0 + ms(3200)), WatchAction::Idle);
		// A path never proven is never judged.
		assert_eq!(assess(None, Some(t0), None, t0 + ms(9000)), WatchAction::Idle);
	}

	#[test]
	fn long_silences_probe_slowly() {
		let t0 = Instant::now();
		assert_eq!(assess(Some(t0), None, Some(t0 + ms(11_000)), t0 + ms(11_600)), WatchAction::Idle);
		assert_eq!(
			assess(Some(t0), None, Some(t0 + ms(11_000)), t0 + ms(13_100)),
			WatchAction::Probe { relay: true }
		);
	}

	#[test]
	fn the_upgrade_hold_down_grows_per_fallback() {
		let t0 = Instant::now();
		let mut w = PathWatch::default();
		assert!(!upgrade_held(&w, t0), "never fell back: nothing held");
		w.fell_back_at = Some(t0);
		w.fallbacks = 1;
		assert!(upgrade_held(&w, t0 + ms(9_900)));
		assert!(!upgrade_held(&w, t0 + ms(10_100)));
		w.fallbacks = 3;
		assert!(upgrade_held(&w, t0 + ms(39_000)));
		assert!(!upgrade_held(&w, t0 + ms(40_100)));
		w.fallbacks = 50;
		assert!(!upgrade_held(&w, t0 + ms(160_100)), "capped at 16x");
	}

	#[test]
	fn rebinding_waits_for_the_current_address_to_go_quiet() {
		let t0 = Instant::now();
		assert!(!may_rebind(Some(t0), t0 + ms(300)), "the old address still delivers");
		assert!(may_rebind(Some(t0), t0 + ms(1000)));
		assert!(may_rebind(None, t0));
	}

	#[test]
	fn relay_data_means_the_peer_moved_only_after_a_direct_gap() {
		let t0 = Instant::now();
		assert!(!peer_moved(Some(t0), t0 + ms(400)), "direct still delivers: the peer just mixes paths");
		assert!(peer_moved(Some(t0), t0 + ms(1000)));
		assert!(!peer_moved(None, t0 + ms(9000)), "a path never proven isn't judged");
	}

	#[test]
	fn link_stamps_never_move_back() {
		let s = LinkStamps::new();
		assert!(s.direct().is_none() && s.relay().is_none());
		let t = Instant::now() + ms(500);
		s.mark_direct(t);
		s.mark_direct(t - ms(400));
		let got = s.direct().unwrap();
		assert!(got >= t - ms(1) && got <= t, "kept the newer stamp");
		s.mark_relay(t);
		assert!(s.relay().is_some());
	}

	#[test]
	fn a_much_faster_relay_wins() {
		assert!(prefer_relay(&[480.0, 520.0, 510.0], &[31.0, 29.0, 35.0]));
	}

	#[test]
	fn near_ties_and_a_faster_direct_path_stay_direct() {
		assert!(!prefer_relay(&[40.0, 42.0], &[30.0, 33.0]), "10 ms better is within the margin");
		assert!(!prefer_relay(&[60.0], &[45.0]), "15 ms / 25 % better is not enough");
		assert!(!prefer_relay(&[12.0, 11.0], &[35.0, 36.0]));
	}

	#[test]
	fn no_relay_answer_keeps_direct_and_a_silent_direct_path_yields() {
		assert!(!prefer_relay(&[300.0], &[]), "an older peer never answers relay probes");
		assert!(!prefer_relay(&[], &[]));
		assert!(prefer_relay(&[], &[40.0]), "direct proven by the punch but silent since");
	}
}
