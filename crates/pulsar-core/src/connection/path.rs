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
}

#[cfg(test)]
mod tests {
	use super::prefer_relay;

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
