//! Inbound datagram dispatch: hole-punch transport establishment plus the relay
//! and peer message handlers that drive a [`Node`]'s session state machine.

use super::*;

impl Node {
	pub(super) async fn establish_transport(
		self: &Arc<Self>,
		session: SessionId,
	) -> Result<Transport, ConnError> {
		if self.mode == NetworkMode::RelayOnly {
			return Ok(Transport::Relay);
		}
		let (peer_addr, peer_lan) = match self.inner.lock().await.sessions.get(&session) {
			Some(s) => (s.peer_addr, s.peer_lan),
			None => return Err(ConnError::P2pFailed),
		};
		tracing::info!(session, peer = %peer_addr, lan = ?peer_lan, "p2p: punching the peer");
		let started = std::time::Instant::now();

		let pk = Arc::new(Notify::new());
		self.inner.lock().await.punched.insert(session, pk.clone());
		let mut seq = 0u32;
		// Opening burst. Same-NAT peer: `send_punch` also punches its PRIVATE candidate —
		// the public addr is a router hairpin there (lossy); whichever path answers first
		// wins `peer_addr` (LAN preferred, see the Punch handler).
		while seq < PUNCH_ATTEMPTS as u32 {
			self.send_punch(session, seq, peer_addr, peer_lan).await;
			seq += 1;
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		// Then keep retransmitting while we wait: the peer's own punches open its NAT for us
		// only once they leave, and the first ones are often lost to that race.
		let deadline = tokio::time::Instant::now() + PUNCH_TIMEOUT;
		let acked = loop {
			let left = deadline.saturating_duration_since(tokio::time::Instant::now());
			if left.is_zero() {
				break false;
			}
			if timeout(left.min(PUNCH_RETRY), pk.notified()).await.is_ok() {
				break true;
			}
			self.send_punch(session, seq, peer_addr, peer_lan).await;
			seq += 1;
		};
		// The waiter is only needed during the punch — remove it on every path so the
		// `punched` map doesn't grow one stale `Arc<Notify>` per session, forever.
		self.inner.lock().await.punched.remove(&session);
		let ms = started.elapsed().as_millis() as u64;
		if acked {
			tracing::info!(session, ms, "p2p: direct path answered");
			return Ok(Transport::Direct);
		}
		if self.mode != NetworkMode::Auto {
			tracing::info!(session, ms, "p2p: no direct answer — P2P-only mode, giving up");
			return Err(ConnError::P2pFailed);
		}
		tracing::info!(
			session,
			ms,
			"p2p: no direct answer yet — starting on the relay, punching on in the background"
		);
		// A direct path proven later should still be compared against the relay (path.rs).
		if let Some(s) = self.inner.lock().await.sessions.get_mut(&session) {
			s.measure_on_direct = true;
		}
		self.punch_in_background(session, seq);
		Ok(Transport::Relay)
	}

	/// One `Punch` to the peer's public candidate (+ its LAN candidate, when it gave one).
	pub(super) async fn send_punch(
		&self,
		session: SessionId,
		seq: u32,
		peer_addr: SocketAddr,
		peer_lan: Option<SocketAddr>,
	) {
		let msg = encode(&PeerMsg::Punch { session, seq });
		let _ = self.sock.send_to(&msg, peer_addr).await;
		if let Some(lan) = peer_lan {
			let _ = self.sock.send_to(&msg, lan).await;
		}
	}

	/// Keep punching a session that is (still) on the relay for [`BACKGROUND_PUNCH`]: every
	/// 250 ms for the first seconds, then every second. Stops as soon as the direct path is
	/// proven (the Punch/PunchAck handlers then move the session's traffic over) or the
	/// session ends. Both ends run it, so their NATs open towards each other in step even
	/// when the first packets crossed in the wrong order.
	///
	/// After [`BACKGROUND_PUNCH`] it keeps going every [`SLOW_PUNCH`] for the rest of the
	/// session (one tiny packet): a path that was blocked at connect time (or one the watchdog
	/// just left, see path.rs) can come back mid-session, and the session should move back to it.
	/// A fallback starts a fresh loop and bumps `punch_gen`, which retires the older one.
	pub(super) fn punch_in_background(self: &Arc<Self>, session: SessionId, first_seq: u32) {
		let weak = Arc::downgrade(self);
		tokio::spawn(async move {
			let Some(node) = weak.upgrade() else { return };
			let gen = match node.inner.lock().await.sessions.get(&session) {
				Some(s) => s.watch.punch_gen,
				None => return,
			};
			drop(node);
			let started = std::time::Instant::now();
			let mut seq = first_seq;
			let mut told = false;
			loop {
				let gap = if started.elapsed() < Duration::from_secs(4) {
					Duration::from_millis(250)
				} else if started.elapsed() < BACKGROUND_PUNCH {
					Duration::from_secs(1)
				} else {
					SLOW_PUNCH
				};
				tokio::time::sleep(gap).await;
				let Some(node) = weak.upgrade() else { return };
				let (addr, lan) = match node.inner.lock().await.sessions.get(&session) {
					Some(s) if !s.direct_ok && s.watch.punch_gen == gen => (s.peer_addr, s.peer_lan),
					_ => return, // direct already, a newer loop took over, or the session is gone
				};
				if !told && started.elapsed() >= BACKGROUND_PUNCH {
					told = true;
					tracing::info!(
						session,
						secs = BACKGROUND_PUNCH.as_secs(),
						every_secs = SLOW_PUNCH.as_secs(),
						"p2p: no direct path found — the session stays on the relay, retrying slowly"
					);
				}
				node.send_punch(session, seq, addr, lan).await;
				seq = seq.wrapping_add(1) % super::path::PROBE_SEQ;
			}
		});
	}

	/// Punch a direct path to a known addr. Never relay-falls-back (direct-IP has no
	/// relay). Best-effort: on a LAN the punch ack arrives; if not, data still flows
	/// to the user-provided (reachable) address.
	pub(super) async fn establish_transport_direct(
		&self,
		session: SessionId,
		peer_addr: SocketAddr,
	) {
		let pk = Arc::new(Notify::new());
		self.inner.lock().await.punched.insert(session, pk.clone());
		for seq in 0..PUNCH_ATTEMPTS as u32 {
			let _ = self
				.sock
				.send_to(&encode(&PeerMsg::Punch { session, seq }), peer_addr)
				.await;
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		let _ = timeout(PUNCH_TIMEOUT, pk.notified()).await;
		// Remove the waiter regardless of outcome so the `punched` map doesn't leak one
		// stale `Arc<Notify>` per direct connect.
		self.inner.lock().await.punched.remove(&session);
	}

	pub(super) async fn handle_datagram(self: &Arc<Self>, buf: &[u8], from: SocketAddr) {
		if from == self.relay {
			if let Ok(msg) = decode::<RelayMsg>(buf) {
				self.handle_relay(msg).await;
			}
		} else if let Ok(msg) = decode::<PeerMsg>(buf) {
			self.handle_peer(msg, from).await;
		}
	}

	async fn handle_relay(self: &Arc<Self>, msg: RelayMsg) {
		match msg {
			RelayMsg::Registered {
				id,
				token,
				e2e_required,
				auth_key,
			} => {
				let mut g = self.inner.lock().await;
				// A re-register (NotRegistered → register_msg) after a relay restart
				// that lost `by_pubkey` mints a DIFFERENT id. Detect that rotation so
				// the app can re-advertise it; the UI/LAN beacon snapshot the id once
				// and would otherwise keep broadcasting the dead old one forever.
				let rotated = matches!(g.self_id, Some(prev) if prev != id);
				g.self_id = Some(id);
				g.token = Some(token);
				g.e2e_required = e2e_required;
				// The relay issues a key only on the registration that just passed the
				// interactive factors; a key-authenticated one carries None and must NOT
				// clear the key we already hold.
				if auth_key.is_some() {
					g.issued_key = auth_key;
				}
				// A successful registration clears any pending auth challenge.
				g.auth_required = None;
				// Clear any stale register error: we're registered now.
				g.register_error = None;
				drop(g);
				self.registered.notify_waiters();
				self.registered.notify_one();
				if rotated {
					// notify_one stores a permit, so a rotation that lands while the
					// watcher is mid-processing (not parked in `notified()`) isn't lost.
					self.id_changed.notify_one();
				}
			}
			RelayMsg::HeartbeatAck => {}
			RelayMsg::Incoming {
				from,
				from_addr,
				session,
				hello,
			} => self.on_incoming(from, from_addr, session, hello).await,
			RelayMsg::PeerFound {
				target: _,
				target_addr,
				session,
				answer,
				rate_cap_kbps,
			} => {
				// answer = peer pubkey(32) || peer salt(32) || LAN slot || v1 extension
				// (eph + caps). Reject a malformed blob rather than index-slicing past the end.
				let Some((key, peer_hs, peer_caps)) = split_handshake(&answer) else {
					tracing::warn!(session, "PeerFound answer blob too short");
					return;
				};
				let peer_lan = parse_lan_candidate(&answer);
				let mut g = self.inner.lock().await;
				// If the caller pinned the expected identity for this id (TOFU /
				// known-host), DROP a PeerFound whose key differs: the relay only
				// proves it enforced `target == id`, NOT which pubkey owns that id, so
				// a malicious/compromised relay (the stated threat model) or an
				// attacker that won the pubkey→id registration race could otherwise
				// answer in the target's place and terminate the E2E with its own key.
				// Leave the waiter/salt untouched so a legitimate retransmitted
				// PeerFound can still complete within the connect timeout; if none
				// arrives the connect fails `TargetUnreachable` (mirrors `HelloAck`).
				if let Some(&expected) = g.expected_pubkey.get(&session) {
					if key != expected {
						tracing::warn!(
							session,
							"PeerFound pubkey != expected (pinned) key — dropping (relay/MITM identity mismatch)"
						);
						// Signal the mismatch flag so `connect_pinned` can return
						// `IdentityChanged` instead of the generic `TargetUnreachable`.
						if let Some(flag) = g.identity_mismatch.get(&session) {
							flag.store(true, std::sync::atomic::Ordering::Release);
						}
						return;
					}
				}
				// Pair the peer's salt with the one we minted in `connect`.
				let Some(our_hs) = g.pending_salt.remove(&session) else {
					tracing::warn!(session, "PeerFound for an unknown/expired connect");
					return;
				};
				// The key is accepted (matched the pin, or none was set) — the pin has
				// served its purpose for this session.
				g.expected_pubkey.remove(&session);
				let crypto =
					self.identity
						.session_hs(key, Role::Initiator, session, &our_hs, &peer_hs);
				g.sessions.insert(
					session,
					SessionState {
						peer_addr: target_addr,
						peer_lan,
						crypto,
						peer_pubkey: key,
						transport: Transport::Relay,
						send_seq: 0,
						direct_ok: false,
						data_tx: None,
						our_hs,
						peer_caps,
						path_checked: false,
						relay_preferred: false,
						measure_on_direct: false,
						path_ms: None,
						has_relay: true,
						stamps: super::path::LinkStamps::new(),
						watch: Default::default(),
					},
				);
				// Stash the relay's per-session cap (kbit/s, 0 = uncapped) so `connect` can copy
				// it onto the returned Session — used to clamp the encoder when this session ends
				// up relayed (the relay forwards the media, so its per-session cap applies).
				g.pending_rate_cap.insert(session, rate_cap_kbps);
				if let Some(n) = g.peer_found.get(&session).cloned() {
					n.notify_one();
				}
			}
			RelayMsg::RelayData { session, payload } => {
				// Relay-fallback inbound: unwrap the tunnelled PeerMsg.
				// Move the decoded payload into `deliver` so it can decrypt in place.
				// Punch/PunchAck through the relay are the `Auto` path-latency probes (path.rs) —
				// and proof the relay path is alive (the mid-session watchdog's fallback input).
				match decode::<PeerMsg>(&payload) {
					Ok(PeerMsg::Data { seq, payload, .. }) => {
						self.deliver(session, seq, payload, None).await
					}
					Ok(PeerMsg::Punch { seq, .. }) => {
						self.relay_heard(session).await;
						self.answer_relay_probe(session, seq).await
					}
					Ok(PeerMsg::PunchAck { seq, .. }) => {
						self.relay_heard(session).await;
						self.record_probe(session, seq).await
					}
					_ => {}
				}
			}
			RelayMsg::AuthRequired {
				password,
				totp,
				nonce,
			} => {
				// v4: the relay has an auth policy and wants a credential. Two cases:
				let g = self.inner.lock().await;
				if g.self_id.is_none() {
					// Initial registration in flight — hand the challenge to `register()`,
					// which builds the proof from our credentials (or fails with
					// `RelayAuthRequired` so the app can prompt for exactly what's missing).
					let mut g = g;
					g.auth_required = Some((password, totp, nonce));
					drop(g);
					self.registered.notify_waiters();
					self.registered.notify_one();
				} else {
					// A heartbeat-driven recovery re-register (the relay dropped us, e.g. a
					// restart) got challenged. Auto-answer with a proof when we can — i.e. the
					// password factor is satisfiable and no fresh TOTP is demanded (TOTP is
					// one-shot; a relay requiring it per re-register is re-authenticated by
					// the app on its next go_online, not here).
					drop(g);
					let can_answer = {
						let creds = self.relay_creds.lock().unwrap();
						!totp && (!password || creds.password.is_some())
					};
					if can_answer {
						let proof = self.build_auth_proof(nonce);
						let _ = self
							.sock
							.send_to(&encode(&self.register_msg_with(Some(proof))), self.relay)
							.await;
					} else {
						tracing::warn!("relay re-auth needed but no usable credentials — awaiting app re-login");
					}
				}
			}
			RelayMsg::Error { code, message } => {
				tracing::warn!(?code, message, "relay error");
				// A relay refusal that arrives while we're still UNREGISTERED (initial
				// `register()` in flight, no id yet) is fatal for registration — most
				// notably `IncompatibleVersion`. Record it and wake the `register()`
				// waiter so it fails with a specific `ConnError` (e.g. a clear "update
				// required" message) instead of waiting out the full `REGISTER_TIMEOUT`
				// and reporting a generic `RelayTimeout`.
				{
					let mut g = self.inner.lock().await;
					if g.self_id.is_none() {
						g.register_error = Some(code);
						drop(g);
						self.registered.notify_waiters();
						self.registered.notify_one();
					} else if code == ErrCode::IncompatibleVersion
						|| (code == ErrCode::Protocol && message == "incompatible protocol version")
					{
						// Already registered but the relay now rejects a re-Register with a
						// version-mismatch error (relay was redeployed with a newer
						// PROTOCOL_VERSION). The heartbeat will keep bouncing; every future
						// Register attempt will be refused too. Signal the version-error
						// watcher so the app can surface an "update required" message and go
						// offline — instead of silently advertising a dead id forever.
						//
						// IMPORTANT — only treat `ErrCode::Protocol` as a version error when
						// the message is exactly "incompatible protocol version" (the relay's
						// dedicated string for Register-version rejections). The relay also
						// emits `ErrCode::Protocol` for benign stale-session conditions on
						// RelayData/Accept ("no such session", "not a session member",
						// "not the session target") — those arrive in relay-fallback sessions
						// whenever the relay restarts and loses its in-memory sessions map or
						// the session is GC'd at SESSION_TTL. Treating those as a version
						// error would falsely strand a still-registered host offline with a
						// permanent "update required" banner.
						//
						// Wire-compat note: the relay emits `ErrCode::Protocol` (not
						// `IncompatibleVersion`) for version-mismatch so old builds that
						// pre-date `IncompatibleVersion` can decode the error. We match both
						// so the future two-phase rollout (flip the relay to `IncompatibleVersion`
						// once all deployed clients can decode it) needs no client change.
						drop(g);
						self.version_error.notify_one();
					}
				}
				// The relay evicted us (>TTL outage / relay restart): our id+token
				// are stale, every heartbeat keeps bouncing, and this device is
				// unreachable by ID until re-registered. Re-send Register — if the
				// relay still has our pubkey → id mapping it reissues the SAME
				// 9-digit ID; but a full relay restart loses `by_pubkey` and mints
				// a DIFFERENT id, so the `Registered` arm signals `id_changed` and
				// the app re-advertises the new id. Bounded by the heartbeat
				// cadence: at most one retry per bounce.
				if code == ErrCode::NotRegistered {
					let _ = self
						.sock
						.send_to(&encode(&self.register_msg()), self.relay)
						.await;
				}
			}
		}
	}

	async fn on_incoming(
		self: &Arc<Self>,
		from: DeviceId,
		from_addr: SocketAddr,
		session: SessionId,
		hello: Vec<u8>,
	) {
		// hello = requester pubkey(32) || requester salt(32) || optional LAN
		// candidate. Reject a malformed blob.
		let Some((key, peer_hs, peer_caps)) = split_handshake(&hello) else {
			tracing::warn!(session, "incoming hello blob too short");
			return;
		};
		let peer_lan = parse_lan_candidate(&hello);
		// Retransmit guard: a duplicated requester Connect (UDP can duplicate) makes the
		// relay emit a 2nd Incoming. If this session already exists, re-send Accept with
		// the SAME salt we already committed — minting a fresh salt would derive a
		// mismatched key (the requester only derives once) — and do NOT overwrite the
		// session or emit a second Session (mirrors the direct-IP Hello retransmit guard).
		{
			let g = self.inner.lock().await;
			if let Some(s) = g.sessions.get(&session) {
				let answer = self.handshake_blob(&s.our_hs);
				let (id, token) = (g.self_id, g.token);
				drop(g);
				if let (Some(id), Some(token)) = (id, token) {
					let _ = self
						.sock
						.send_to(
							&encode(&ClientMsg::Accept {
								id,
								token,
								session,
								answer,
							}),
							self.relay,
						)
						.await;
				}
				return;
			}
		}
		// Mint our salt; both salts + the session id bind this specific session's key.
		let our_hs = LocalHs::new();
		let crypto = self
			.identity
			.session_hs(key, Role::Responder, session, &our_hs, &peer_hs);
		let answer = self.handshake_blob(&our_hs);
		let (data_tx, data_rx) = mpsc::unbounded_channel();

		// Read self_id + token BEFORE inserting the session so we never leak a
		// dangling SessionState + mpsc channel when the node is unregistered.
		// (An attacker-chosen session id on an unregistered node would otherwise
		// leave one SessionState per packet, growing unboundedly — conn-r15-2.)
		let (id, token) = {
			let g = self.inner.lock().await;
			(g.self_id, g.token)
		};
		let (Some(id), Some(token)) = (id, token) else {
			return;
		};

		{
			let mut g = self.inner.lock().await;
			g.sessions.insert(
				session,
				SessionState {
					peer_addr: from_addr,
					peer_lan,
					crypto,
					peer_pubkey: key,
					transport: Transport::Relay,
					send_seq: 0,
					direct_ok: false,
					data_tx: Some(data_tx),
					our_hs,
					peer_caps,
					path_checked: false,
					relay_preferred: false,
					measure_on_direct: true,
					path_ms: None,
					has_relay: true,
					stamps: super::path::LinkStamps::new(),
					watch: Default::default(),
				},
			);
		}

		// Accept: send back our handshake blob (minted above) as the answer, then punch.
		let _ = self
			.sock
			.send_to(
				&encode(&ClientMsg::Accept {
					id,
					token,
					session,
					answer,
				}),
				self.relay,
			)
			.await;
		// RelayOnly "skips punching entirely" (don't reveal our addr); the relay
		// carries the traffic. Other modes probe to upgrade to a direct path —
		// including the peer's PRIVATE candidate (same-NAT: the public addr is a
		// lossy router hairpin; the LAN path wins via the Punch-handler preference).
		// Paced like establish_transport's burst, in its own task: the first probes
		// beat the requester's PeerFound by one relay RTT and are dropped (no
		// session there yet) — the paced retransmits land after it exists, and
		// recv_loop keeps draining meanwhile.
		if self.mode != NetworkMode::RelayOnly {
			tracing::info!(session, peer = %from_addr, lan = ?peer_lan, "p2p: punching the requester");
			let node = self.clone();
			tokio::spawn(async move {
				for seq in 0..PUNCH_ATTEMPTS as u32 {
					node.send_punch(session, seq, from_addr, peer_lan).await;
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
				node.punch_in_background(session, PUNCH_ATTEMPTS as u32);
			});
		}

		let _ = self.incoming_tx.send(Session {
			id: session,
			peer: from,
			transport: Transport::Relay,
			node: self.clone(),
			data_rx,
			rate_cap_kbps: 0, // host side accepts; the client is the one that requests bitrate
			pake_client: None,
		});
	}

	async fn handle_peer(self: &Arc<Self>, msg: PeerMsg, from: SocketAddr) {
		match msg {
			PeerMsg::Punch { session, seq } => {
				// A RelayOnly node keeps the session on the relay: it never punches, and it
				// must NOT ack a peer's Punch either. An ack lets an Auto/P2pOnly peer's
				// PunchAck arm flip ITS OWN transport to Direct and start sending media
				// straight to us — bypassing the relay this side explicitly chose (and
				// direct_ok latches, so it never falls back). So suppress the ack AND skip
				// adopting Direct locally when RelayOnly.
				if self.mode != NetworkMode::RelayOnly {
					let _ = self
						.sock
						.send_to(&encode(&PeerMsg::PunchAck { session, seq }), from)
						.await;
					let mut g = self.inner.lock().await;
					if let Some(s) = g.sessions.get_mut(&session) {
						// A session that just fell back from a dead direct path keeps it unused for
						// a hold-down even when a punch proves it again (path.rs `upgrade_held`):
						// still acked above, so the PEER can see the path; we wait.
						let held = !s.direct_ok
							&& super::path::upgrade_held(&s.watch, std::time::Instant::now());
						// SECURITY: only adopt the data path if `from` is one of this
						// session's known punch candidates (the relay-observed public addr
						// or the LAN candidate we were given during rendezvous). Without
						// this gate, any party that learns the SessionId (a compromised
						// relay, an on-LAN sniff) can redirect the victim's data path to
						// itself and suppress relay fallback. Mirrors the PunchAck guard
						// at the PunchAck arm below.
						if let Some(reflexive) = punch_candidate(s, from).filter(|_| !held) {
							// PREFER THE PRIVATE PATH: with both the LAN route and the
							// router-hairpin route alive, punch retransmits would otherwise
							// flap `peer_addr` between them — and the hairpin route is the
							// lossy one. Once a private source is locked in, a public
							// arrival no longer overwrites it.
							if !(s.direct_ok && is_private_v4(s.peer_addr)) || is_private_v4(from) {
								s.peer_addr = from;
							}
							if !s.direct_ok {
								tracing::info!(session, via = %from, reflexive, "p2p: direct path established (peer's punch arrived)");
								// Stamped when the path is adopted (the watchdog judges only a proven
								// path). Later punches are NOT liveness: they are unauthenticated and
								// prove only peer -> us. A peer that fell back off a path dead towards
								// it punches us every 250 ms, and counting those kept our watchdog
								// from ever seeing the silence — our video went on into the dead path.
								s.stamps.mark_direct(std::time::Instant::now());
							}
							s.direct_ok = true;
							if !s.relay_preferred {
								s.transport = Transport::Direct;
							}
							if s.measure_on_direct && !s.path_checked && self.mode == NetworkMode::Auto {
								let node = self.clone();
								tokio::spawn(async move {
									node.pick_faster_path(session).await;
								});
							}
						}
					}
				}
			}
			PeerMsg::PunchAck { session, seq } => {
				// Mirror of the Punch arm's mode guard: a RelayOnly node never
				// punches, so any ack is gratuitous — and must not flip Direct.
				if self.mode == NetworkMode::RelayOnly {
					return;
				}
				self.record_probe(session, seq).await;
				let mut g = self.inner.lock().await;
				let Some(s) = g.sessions.get_mut(&session) else {
					return;
				};
				// The answer to a NAT-rebinding check (path.rs `maybe_rebind`): the new address is
				// not a punch candidate, so this comes first.
				if Node::rebind_answered(s, from, seq, std::time::Instant::now()) {
					return;
				}
				// Only honor acks from one of THIS session's punch candidates:
				// every Pulsar binds the same well-known port and acks probes for
				// unknown sessions, so a stale private candidate can resolve to a
				// DIFFERENT device on our LAN — its ack would lock Direct while
				// `peer_addr` still points at an unpunchable public addr,
				// suppressing the relay fallback.
				let Some(reflexive) = punch_candidate(s, from) else {
					return;
				};
				// Fallback hold-down (see the Punch arm): the path answers again, but stays unused
				// for now; the background punch keeps proving it until the hold-down ends.
				if !s.direct_ok && super::path::upgrade_held(&s.watch, std::time::Instant::now()) {
					return;
				}
				// The ack's source IS a verified working path — adopt it, with the
				// same prefer-the-private-path rule as the Punch arm (otherwise the
				// initiator keeps the relay-observed public addr and its data
				// blackholes on non-hairpin NATs / rides the lossy hairpin).
				if !(s.direct_ok && is_private_v4(s.peer_addr)) || is_private_v4(from) {
					s.peer_addr = from;
				}
				if !s.direct_ok {
					tracing::info!(session, via = %from, reflexive, "p2p: direct path established (our punch was answered)");
				}
				// An answer from the session's direct address is liveness for the watchdog (its
				// own probes come back here) — the adoption, or an ack of a probe-range seq (the
				// watchdog's / path choice's). Not a late ack of an old hole-punch burst.
				if from == s.peer_addr && (!s.direct_ok || seq >= super::path::PROBE_SEQ) {
					s.stamps.mark_direct(std::time::Instant::now());
				}
				s.direct_ok = true;
				if !s.relay_preferred {
					s.transport = Transport::Direct;
				}
				if s.measure_on_direct && !s.path_checked && self.mode == NetworkMode::Auto {
					let node = self.clone();
					tokio::spawn(async move {
						node.pick_faster_path(session).await;
					});
				}
				if let Some(n) = g.punched.get(&session).cloned() {
					n.notify_one();
				}
			}
			PeerMsg::Data {
				session,
				seq,
				payload,
			} => self.deliver(session, seq, payload, Some(from)).await,
			PeerMsg::KeepAlive { .. } => {}
			PeerMsg::Hello {
				session,
				pubkey,
				salt,
			} => {
				self.on_hello(from, session, pubkey, PeerHs { salt, eph: None }, 0)
					.await
			}
			PeerMsg::Hello2 {
				session,
				pubkey,
				salt,
				eph,
				caps,
			} => {
				self.on_hello(
					from,
					session,
					pubkey,
					PeerHs {
						salt,
						eph: Some(eph),
					},
					caps,
				)
				.await
			}
			PeerMsg::HelloAck {
				session,
				pubkey,
				salt,
			} => {
				self.on_hello_ack(from, session, pubkey, PeerHs { salt, eph: None }, 0)
					.await
			}
			PeerMsg::HelloAck2 {
				session,
				pubkey,
				salt,
				eph,
				caps,
			} => {
				self.on_hello_ack(
					from,
					session,
					pubkey,
					PeerHs {
						salt,
						eph: Some(eph),
					},
					caps,
				)
				.await
			}
		}
	}

	/// Direct-IP `Hello` / `Hello2` (inbound connect, no relay): derive crypto as
	/// Responder, reply with our key + salt (+ eph for `Hello2`), punch, and emit a
	/// Session via next_incoming.
	async fn on_hello(
		self: &Arc<Self>,
		from: SocketAddr,
		session: SessionId,
		pubkey: PublicKey,
		peer_hs: PeerHs,
		peer_caps: u8,
	) {
		// Inbound direct-IP connect (no relay): derive crypto as Responder,
		// reply with our key + salt, punch, and emit a Session via next_incoming.
		let mut g = self.inner.lock().await;
		// SECURITY (conn-r15-1): reject a Hello whose session id matches one of
		// OUR in-flight outbound connect_direct attempts.  An on-LAN attacker
		// that snoops (or receives) our Hello{session, our_pk, our_salt} knows
		// the random session id and can race us with its own Hello{session,
		// attacker_pk, ...}; without this gate our Hello handler would derive
		// Responder crypto against the attacker's key, insert a session, and
		// emit a Session — completing a full MITM before any pin check runs.
		// `pending_salt` and `hello_done` together uniquely identify a session
		// id we own as the initiator; a legitimate inbound initiator from a
		// different machine cannot have either entry for a freshly-random id.
		if g.pending_salt.contains_key(&session) || g.hello_done.contains_key(&session) {
			tracing::warn!(
				session,
				"inbound Hello for an in-flight outbound connect_direct — dropping (possible MITM)"
			);
			return;
		}
		if let Some(s) = g.sessions.get(&session) {
			// Hello retransmit — re-ack with the SAME salt we already committed
			// to (a different salt would derive a mismatched key), so a lost
			// HelloAck still recovers.
			// Re-ack in the form the session was created with (v2 ⇔ forward secret).
			let ack = self.hello_ack(session, &s.our_hs, s.crypto.is_forward_secret());
			drop(g);
			let _ = self.sock.send_to(&ack, from).await;
			return;
		}
		let our_hs = LocalHs::new();
		let crypto = self
			.identity
			.session_hs(pubkey, Role::Responder, session, &our_hs, &peer_hs);
		let ack = self.hello_ack(session, &our_hs, peer_hs.eph.is_some());
		let (data_tx, data_rx) = mpsc::unbounded_channel();
		g.sessions.insert(
			session,
			SessionState {
				peer_addr: from,
				peer_lan: None, // direct-IP: the typed address IS the path
				crypto,
				peer_pubkey: pubkey,
				transport: Transport::Direct,
				send_seq: 0,
				direct_ok: false,
				data_tx: Some(data_tx),
				our_hs,
				peer_caps,
				path_checked: true, // direct-IP: no relay path to compare
				relay_preferred: false,
				measure_on_direct: false,
				path_ms: None,
				has_relay: false,
				stamps: super::path::LinkStamps::new(),
				watch: Default::default(),
			},
		);
		drop(g);
		let _ = self.sock.send_to(&ack, from).await;
		for seq in 0..PUNCH_ATTEMPTS as u32 {
			let _ = self
				.sock
				.send_to(&encode(&PeerMsg::Punch { session, seq }), from)
				.await;
		}
		let _ = self.incoming_tx.send(Session {
			id: session,
			peer: DeviceId(0),
			transport: Transport::Direct,
			node: self.clone(),
			data_rx,
			rate_cap_kbps: 0, // direct-IP host accept: no relay, no cap
			pake_client: None,
		});
	}

	/// Direct-IP `HelloAck` / `HelloAck2`: the initiator derives the key.
	async fn on_hello_ack(
		self: &Arc<Self>,
		from: SocketAddr,
		session: SessionId,
		pubkey: PublicKey,
		peer_hs: PeerHs,
		peer_caps: u8,
	) {
		let mut g = self.inner.lock().await;
		if !g.sessions.contains_key(&session) {
			// If the caller pinned the peer's key (beacon/known-host path),
			// DROP an ack carrying any other key: an attacker that answers our
			// Hello in the peer's place would otherwise terminate the E2E with
			// its own key (silent MITM). We don't notify the waiter or consume
			// our salt, so a legitimate retransmitted ack can still complete
			// within the connect timeout; if none arrives it fails P2pFailed.
			if let Some(&expected) = g.expected_pubkey.get(&session) {
				if pubkey != expected {
					tracing::warn!(
						session,
						"HelloAck pubkey != expected (pinned) key — dropping (possible MITM)"
					);
					// Signal the mismatch flag so the caller can return
					// `IdentityChanged` instead of the generic `P2pFailed`.
					if let Some(flag) = g.identity_mismatch.get(&session) {
						flag.store(true, std::sync::atomic::Ordering::Release);
					}
					return;
				}
			}
			// Pair the peer's salt with the one we minted in `connect_direct`.
			let Some(our_hs) = g.pending_salt.remove(&session) else {
				tracing::warn!(session, "HelloAck for an unknown/expired direct connect");
				return;
			};
			// The key is accepted (matched the pin, or none was set) — the
			// pin has served its purpose for this session.
			g.expected_pubkey.remove(&session);
			let crypto =
				self.identity
					.session_hs(pubkey, Role::Initiator, session, &our_hs, &peer_hs);
			g.sessions.insert(
				session,
				SessionState {
					peer_addr: from,
					peer_lan: None, // direct-IP: the typed address IS the path
					crypto,
					peer_pubkey: pubkey,
					transport: Transport::Direct,
					send_seq: 0,
					direct_ok: false,
					data_tx: None,
					our_hs,
					peer_caps,
					path_checked: true, // direct-IP: no relay path to compare
					relay_preferred: false,
					measure_on_direct: false,
					path_ms: None,
					has_relay: false,
					stamps: super::path::LinkStamps::new(),
					watch: Default::default(),
				},
			);
		} else {
			// SECURITY (conn-r15-1, defense-in-depth): the session already
			// exists — meaning the inbound Hello path raced the HelloAck and
			// the MITM guard above (Part 1) didn't fire (e.g. timing edge).
			// If this was a pinned connect, verify the existing session's
			// peer_pubkey matches the pin; on mismatch evict the session so
			// connect_direct returns IdentityChanged instead of handing the
			// caller a session keyed to the attacker.
			if let Some(&expected) = g.expected_pubkey.get(&session) {
				let actual = g.sessions.get(&session).map(|s| s.peer_pubkey);
				if actual != Some(expected) {
					tracing::warn!(
						session,
						"HelloAck: existing session peer_pubkey != pinned expected key — evicting (possible MITM)"
					);
					g.sessions.remove(&session);
					if let Some(flag) = g.identity_mismatch.get(&session) {
						flag.store(true, std::sync::atomic::Ordering::Release);
					}
					return;
				}
			}
		}
		if let Some(n) = g.hello_done.get(&session).cloned() {
			n.notify_one();
		}
	}

	/// Our direct-IP ack: `HelloAck2` (with eph + caps) for a forward-secret session,
	/// the legacy `HelloAck` for an old initiator.
	fn hello_ack(&self, session: SessionId, hs: &LocalHs, v2: bool) -> Vec<u8> {
		if v2 {
			encode(&PeerMsg::HelloAck2 {
				session,
				pubkey: self.identity.public_bytes(),
				salt: hs.salt,
				eph: hs.eph_public(),
				caps: OUR_CAPS,
			})
		} else {
			encode(&PeerMsg::HelloAck {
				session,
				pubkey: self.identity.public_bytes(),
				salt: hs.salt,
			})
		}
	}

	/// Decrypt an inbound payload and hand it to the session's reader.
	///
	/// `crypto.open_in_place` both authenticates the ciphertext **and** enforces
	/// sliding-window anti-replay on `seq` (RFC 6479 style, reorder-tolerant since UDP
	/// datagrams arrive out of order). A failure — bad ciphertext or a replayed/too-old
	/// seq — is silently dropped, which is the right thing for UDP.
	///
	/// Takes the ciphertext **by value** so it can be decrypted in place (B7). The
	/// node lock is held ONLY to clone the (Arc-backed) crypto handle and the data
	/// sender — the AEAD runs AFTER the lock is released, so recv_loop keeps draining
	/// the socket during a keyframe burst instead of blocking behind a full decrypt.
	///
	/// `from` is the direct source (`None` = tunnelled through the relay). An authenticated
	/// packet stamps its path's liveness for the mid-session watchdog (lock-free), and one from a
	/// direct address other than the session's may be a NAT rebinding (`maybe_rebind`, which
	/// takes the lock again — only in that rare case).
	async fn deliver(
		self: &Arc<Self>,
		session: SessionId,
		seq: u64,
		ciphertext: Vec<u8>,
		from: Option<SocketAddr>,
	) {
		let (crypto, tx, stamps, from_current, may_move, may_follow) = {
			let g = self.inner.lock().await;
			match g.sessions.get(&session) {
				Some(s) => (
					s.crypto.clone(),
					s.data_tx.clone(),
					s.stamps.clone(),
					from.is_none_or(|f| f == s.peer_addr),
					// Only a session sending direct has an address to move (`maybe_rebind`).
					s.transport == Transport::Direct,
					// ... or a relay to follow the peer onto (`peer_moved_to_relay`).
					s.has_relay && s.transport == Transport::Direct && s.direct_ok && !s.relay_preferred,
				),
				None => return,
			}
		};
		// Decrypt in place off-lock; `buf` is truncated to the plaintext on success.
		let mut buf = ciphertext;
		if crypto.open_in_place(seq, &mut buf).is_ok() {
			let now = std::time::Instant::now();
			match from {
				None => {
					stamps.mark_relay(now);
					// Checked lock-free per packet; the lock is taken only for the switch itself.
					if may_follow && super::path::peer_moved(stamps.direct(), now) {
						self.peer_moved_to_relay(session, now).await;
					}
				}
				Some(_) if from_current => stamps.mark_direct(now),
				Some(addr) if may_move => self.maybe_rebind(session, addr, now).await,
				Some(_) => {}
			}
			if let Some(tx) = tx {
				let _ = tx.send(buf);
			}
		}
	}
}

/// Is `from` an acceptable source for this session's punch traffic? `Some(false)` for one of
/// the candidates we were given (the relay-observed address, the LAN candidate);
/// `Some(true)` for a PEER-REFLEXIVE source — the same public IP on another port, which is
/// what a peer behind an endpoint-dependent ("symmetric") NAT looks like: its NAT gave its
/// packets to us a different port than the one the relay saw. Only accepted until the direct
/// path is proven, and never for private/loopback IPs (there, another port on the same IP is
/// another Pulsar instance, not the peer). Session ids are 53 random bits known only to the
/// relay and the two peers, so a stray sender on the same carrier IP can't guess one.
fn punch_candidate(s: &SessionState, from: SocketAddr) -> Option<bool> {
	if from == s.peer_addr || Some(from) == s.peer_lan {
		return Some(false);
	}
	let public = !is_private_v4(from) && !from.ip().is_loopback() && !from.ip().is_unspecified();
	(!s.direct_ok && public && from.ip() == s.peer_addr.ip()).then_some(true)
}

#[cfg(test)]
mod tests {
	use super::*;

	const SESSION: SessionId = 42;

	fn addr(s: &str) -> SocketAddr {
		s.parse().unwrap()
	}

	/// A node whose handlers we drive directly — the relay addr is a black hole
	/// (no relay involved) and outbound acks/punches go nowhere; only the state
	/// transitions matter here.
	async fn test_node() -> Arc<Node> {
		Node::bind(
			addr("127.0.0.1:0"),
			addr("198.51.100.1:21116"),
			NetworkMode::Auto,
		)
		.await
		.expect("bind test node")
	}

	/// `PeerFound` answer blob: peer pubkey(32) || peer salt(32) || LAN tail.
	fn answer_with_lan(lan: SocketAddr) -> Vec<u8> {
		let mut blob = vec![7u8; 32];
		blob.extend_from_slice(&[9u8; 32]);
		if let SocketAddr::V4(a) = lan {
			blob.extend_from_slice(&a.ip().octets());
			blob.extend_from_slice(&a.port().to_be_bytes());
		}
		blob
	}

	/// Stand in for `connect()`'s preamble (salt minted before the rendezvous),
	/// then deliver the relay's `PeerFound` so the session exists.
	async fn rendezvous(node: &Arc<Node>, public: SocketAddr, lan: SocketAddr) {
		node.inner
			.lock()
			.await
			.pending_salt
			.insert(SESSION, LocalHs::new());
		node.handle_relay(RelayMsg::PeerFound {
			target: DeviceId(1),
			target_addr: public,
			session: SESSION,
			answer: answer_with_lan(lan),
			rate_cap_kbps: 0,
		})
		.await;
	}

	/// The initiator's punch state machine: an early responder punch (it beats
	/// `PeerFound` by one relay RTT) must not create state; a `PunchAck` is only
	/// honored from this session's punch candidates and ADOPTS the validated
	/// source as the data path, preferring the private (LAN) one over the
	/// public router hairpin.
	#[tokio::test]
	async fn punch_state_machine_adopts_validated_private_path() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118"); // relay-observed peer addr
		let lan = addr("192.168.77.5:21118"); // peer's LAN candidate
		let foreign = addr("192.168.77.99:21118"); // another Pulsar on our LAN

		// Responder punch arriving BEFORE PeerFound: acked, but no session yet.
		node.handle_peer(
			PeerMsg::Punch {
				session: SESSION,
				seq: 0,
			},
			lan,
		)
		.await;
		assert!(!node.inner.lock().await.sessions.contains_key(&SESSION));

		// PeerFound: the session starts on the relay-observed public addr.
		rendezvous(&node, public, lan).await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).expect("session after PeerFound");
			assert_eq!(s.peer_addr, public);
			assert_eq!(s.peer_lan, Some(lan));
			assert!(!s.direct_ok);
		}

		// An ack from a FOREIGN device (a stale candidate resolved to another
		// Pulsar on the well-known port) must not lock Direct.
		node.handle_peer(
			PeerMsg::PunchAck {
				session: SESSION,
				seq: 0,
			},
			foreign,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert!(!s.direct_ok, "foreign ack must not prove the direct path");
			assert_eq!(s.peer_addr, public);
		}

		// Ack from the public (hairpin) path: verified — adopted as the data path.
		node.handle_peer(
			PeerMsg::PunchAck {
				session: SESSION,
				seq: 0,
			},
			public,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert!(s.direct_ok);
			assert_eq!(s.transport, Transport::Direct);
			assert_eq!(s.peer_addr, public, "the ack source becomes the data path");
		}

		// Ack from the LAN candidate: the private path wins over the hairpin.
		node.handle_peer(
			PeerMsg::PunchAck {
				session: SESSION,
				seq: 1,
			},
			lan,
		)
		.await;
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, lan);

		// A late punch retransmit via the public path must NOT flap the locked
		// private path back to the hairpin.
		node.handle_peer(
			PeerMsg::Punch {
				session: SESSION,
				seq: 3,
			},
			public,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert_eq!(s.peer_addr, lan);
			assert!(s.direct_ok);
		}
	}

	/// A Punch from a foreign address (not a known session candidate) must NOT
	/// mutate peer_addr, direct_ok, or transport — closing the session-hijack /
	/// relay-suppression vector (security regression test for conn-1).
	#[tokio::test]
	async fn punch_from_foreign_addr_does_not_adopt_data_path() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118"); // relay-observed peer addr
		let lan = addr("192.168.77.5:21118"); // peer's LAN candidate
		let foreign = addr("10.0.0.77:21118"); // attacker / unrelated device

		// Session is in relay-fallback state (direct_ok==false, transport==Relay).
		rendezvous(&node, public, lan).await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).expect("session after PeerFound");
			assert_eq!(s.peer_addr, public);
			assert!(!s.direct_ok);
			assert_eq!(s.transport, Transport::Relay);
		}

		// Punch arriving from a foreign address: must be ACKed (the send fires)
		// but MUST NOT redirect the data path.
		node.handle_peer(
			PeerMsg::Punch {
				session: SESSION,
				seq: 0,
			},
			foreign,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert_eq!(
				s.peer_addr, public,
				"foreign Punch must not overwrite peer_addr"
			);
			assert!(!s.direct_ok, "foreign Punch must not set direct_ok");
			assert_eq!(
				s.transport,
				Transport::Relay,
				"foreign Punch must not flip transport to Direct"
			);
		}

		// A legitimate Punch from the relay-observed public addr IS allowed.
		node.handle_peer(
			PeerMsg::Punch {
				session: SESSION,
				seq: 1,
			},
			public,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert!(s.direct_ok, "legitimate Punch (public) must set direct_ok");
			assert_eq!(s.transport, Transport::Direct);
			assert_eq!(s.peer_addr, public);
		}

		// And a Punch from the LAN candidate is also legitimate.
		node.handle_peer(
			PeerMsg::Punch {
				session: SESSION,
				seq: 2,
			},
			lan,
		)
		.await;
		{
			let g = node.inner.lock().await;
			let s = g.sessions.get(&SESSION).unwrap();
			assert_eq!(
				s.peer_addr, lan,
				"Punch from LAN candidate should prefer private path"
			);
		}
	}

	/// A peer behind an endpoint-dependent NAT answers from the relay-observed IP but a NEW
	/// port: accepted (and adopted) while the direct path is unproven; once it is proven, another
	/// port no longer moves it, and a different IP never counts.
	#[tokio::test]
	async fn peer_reflexive_port_is_adopted_only_before_the_path_is_proven() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let lan = addr("192.168.77.5:21118");
		rendezvous(&node, public, lan).await;

		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 0 }, addr("198.51.100.9:40000")).await;
		assert!(!node.inner.lock().await.sessions[&SESSION].direct_ok, "another IP never counts");

		let remapped = addr("203.0.113.7:40312");
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 1 }, remapped).await;
		{
			let g = node.inner.lock().await;
			let s = &g.sessions[&SESSION];
			assert!(s.direct_ok);
			assert_eq!(s.peer_addr, remapped, "the reflexive source becomes the data path");
		}

		node.handle_peer(PeerMsg::Punch { session: SESSION, seq: 2 }, addr("203.0.113.7:40999")).await;
		assert_eq!(
			node.inner.lock().await.sessions[&SESSION].peer_addr,
			remapped,
			"a proven path isn't moved by yet another port"
		);
	}

	/// Only a candidate-validated ack wakes the `punched` waiter that gates
	/// `establish_transport` — a foreign ack must leave it parked (so Auto can
	/// still fall back to the relay).
	#[tokio::test]
	async fn punch_ack_notifies_only_from_session_candidates() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let lan = addr("192.168.77.5:21118");
		rendezvous(&node, public, lan).await;

		let pk = Arc::new(Notify::new());
		node.inner.lock().await.punched.insert(SESSION, pk.clone());

		node.handle_peer(
			PeerMsg::PunchAck {
				session: SESSION,
				seq: 0,
			},
			addr("10.0.0.9:21118"),
		)
		.await;
		assert!(
			timeout(Duration::from_millis(50), pk.notified())
				.await
				.is_err(),
			"foreign ack must not unblock the punch waiter"
		);

		node.handle_peer(
			PeerMsg::PunchAck {
				session: SESSION,
				seq: 0,
			},
			public,
		)
		.await;
		assert!(timeout(Duration::from_millis(50), pk.notified())
			.await
			.is_ok());
	}

	/// A relay-rendezvous session with a REAL peer key (so the test can seal data the node
	/// accepts), proven direct via `public`, with relay credentials set (a fallback needs them).
	/// Returns the peer's crypto, the app-side `Session` and the instant the direct path was
	/// proven.
	async fn live_direct_session(
		node: &Arc<Node>,
		public: SocketAddr,
	) -> (Crypto, Session, std::time::Instant) {
		let ours = LocalHs::new();
		{
			let mut g = node.inner.lock().await;
			g.pending_salt.insert(SESSION, ours.clone());
			g.self_id = Some(DeviceId(100_000_001));
			g.token = Some(Token([3u8; 16]));
		}
		let peer_id = Identity::generate();
		let peer_hs = LocalHs::new();
		node.handle_relay(RelayMsg::PeerFound {
			target: DeviceId(1),
			target_addr: public,
			session: SESSION,
			answer: handshake_blob(peer_id.public_bytes(), &peer_hs, None),
			rate_cap_kbps: 0,
		})
		.await;
		let peer_crypto = peer_id.session_hs(
			node.public_key(),
			Role::Responder,
			SESSION,
			&peer_hs,
			&PeerHs {
				salt: ours.salt,
				eph: Some(ours.eph_public()),
			},
		);
		let (data_tx, data_rx) = mpsc::unbounded_channel();
		node.inner.lock().await.sessions.get_mut(&SESSION).unwrap().data_tx = Some(data_tx);
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 0 }, public).await;
		let proven = std::time::Instant::now();
		let sess = Session {
			id: SESSION,
			peer: DeviceId(1),
			transport: Transport::Direct,
			node: node.clone(),
			data_rx,
			rate_cap_kbps: 0,
			pake_client: None,
		};
		assert_eq!(sess.live_transport().await, Transport::Direct);
		(peer_crypto, sess, proven)
	}

	/// Direct path goes silent while the relay still answers → the session moves onto the relay
	/// (no drop, `live_transport` says so); a brief gap never does; the recovered direct path is
	/// taken back only after the hold-down.
	#[tokio::test]
	async fn a_silent_direct_path_falls_back_to_the_relay_and_recovers() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let (_peer, sess, t0) = live_direct_session(&node, public).await;
		let ms = Duration::from_millis;

		// A 1 s gap: nothing happens.
		node.watch_tick(t0 + ms(1000)).await;
		assert_eq!(sess.live_transport().await, Transport::Direct);
		// Silent past DEAD_AFTER but the relay hasn't answered (both paths down): stay.
		node.watch_tick(t0 + ms(2100)).await;
		node.watch_tick(t0 + ms(3500)).await;
		assert_eq!(sess.live_transport().await, Transport::Direct, "no fallback onto an unproven relay");
		// The relay answers during the silence → fall back.
		node.inner.lock().await.sessions[&SESSION].stamps.mark_relay(t0 + ms(3400));
		node.watch_tick(t0 + ms(3600)).await;
		assert_eq!(sess.live_transport().await, Transport::Relay);
		{
			let g = node.inner.lock().await;
			let s = &g.sessions[&SESSION];
			assert!(!s.direct_ok);
			assert_eq!(s.transport, Transport::Relay);
			assert_eq!(s.peer_addr, public, "the direct address is kept for the re-punch");
		}

		// The direct path answers again at once: held down, the session stays on the relay.
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 7 }, public).await;
		node.handle_peer(PeerMsg::Punch { session: SESSION, seq: 8 }, public).await;
		assert_eq!(sess.live_transport().await, Transport::Relay, "no flap inside the hold-down");

		// After the hold-down a proven direct path is taken back.
		node.inner.lock().await.sessions.get_mut(&SESSION).unwrap().watch.fell_back_at =
			std::time::Instant::now().checked_sub(Duration::from_secs(11));
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 9 }, public).await;
		assert_eq!(sess.live_transport().await, Transport::Direct);
		assert!(node.inner.lock().await.sessions[&SESSION].direct_ok);
	}

	/// A quiet session whose direct probes are answered is just idle — it never falls back.
	#[tokio::test]
	async fn an_idle_session_with_an_answering_direct_path_stays_direct() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let (_peer, sess, _t0) = live_direct_session(&node, public).await;
		let ms = Duration::from_millis;
		// Probe answered (the PunchAck stamps the direct path), relay answered too.
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 0x8010_0000 }, public).await;
		let t1 = std::time::Instant::now();
		node.inner.lock().await.sessions[&SESSION].stamps.mark_relay(t1 + ms(100));
		node.watch_tick(t1 + ms(2500)).await;
		assert_eq!(sess.live_transport().await, Transport::Direct, "2.5 s since the answer: only probing");
	}

	/// NAT rebinding: only an AUTHENTICATED, non-replayed packet from a new address starts a move,
	/// only once the old address went quiet, and the move happens only when the new address
	/// answers the path check (an on-path replay with a third party's source can't redirect the
	/// stream there).
	#[tokio::test]
	async fn rebinding_follows_only_authenticated_packets_after_the_old_address_goes_quiet() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let moved = addr("198.51.100.44:52011"); // the peer behind a new carrier mapping
		let (peer, mut sess, _t0) = live_direct_session(&node, public).await;
		let data = |seq: u64, ct: Vec<u8>| PeerMsg::Data {
			session: SESSION,
			seq,
			payload: ct,
		};

		// Authenticated, but the old address is still live: delivered, not followed.
		node.handle_peer(data(1, peer.seal(1, b"one")), moved).await;
		assert_eq!(sess.try_recv().as_deref(), Some(&b"one"[..]));
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, public);

		tokio::time::sleep(Duration::from_millis(1100)).await;
		// Unauthenticated (forged) and replayed packets from the new address: ignored.
		node.handle_peer(data(2, vec![0u8; 40]), moved).await;
		node.handle_peer(data(1, peer.seal(1, b"one")), moved).await;
		assert!(sess.try_recv().is_none());
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, public, "never on unauthenticated packets");

		// A fresh authenticated packet once the old address is quiet: delivered, and the new
		// address is checked — but nothing moves until it answers.
		node.handle_peer(data(3, peer.seal(3, b"three")), moved).await;
		assert_eq!(sess.try_recv().as_deref(), Some(&b"three"[..]));
		let (checked, seq) = node.inner.lock().await.sessions[&SESSION].watch.rebind_check().expect("a path check");
		assert_eq!(checked, moved);
		assert!(seq >= super::super::path::PROBE_SEQ, "a probe-range seq");
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, public, "not before the answer");
		// More media from the candidate: no second check while the first is in flight.
		node.handle_peer(data(4, peer.seal(4, b"four")), moved).await;
		assert_eq!(sess.try_recv().as_deref(), Some(&b"four"[..]));
		assert_eq!(node.inner.lock().await.sessions[&SESSION].watch.rebind_check(), Some((moved, seq)));

		// A wrong seq, or the right one from elsewhere: not an answer.
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: seq ^ 1 }, moved).await;
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq }, addr("198.51.100.45:52011")).await;
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, public);

		// The new address answers: followed.
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq }, moved).await;
		assert_eq!(node.inner.lock().await.sessions[&SESSION].peer_addr, moved);
		assert_eq!(sess.peer_addr().await, Some(moved));
		assert_eq!(sess.live_transport().await, Transport::Direct);
		assert!(node.inner.lock().await.sessions[&SESSION].watch.rebind_check().is_none());
	}

	/// A path dead only towards the peer: the peer falls back onto the relay and punches our
	/// direct address in the background. Those punches must not keep our direct path "alive",
	/// and the peer's authenticated relay data makes us follow it at once (not ~3 s later, after
	/// the peer's app gave up on the silence).
	#[tokio::test]
	async fn a_peer_that_fell_back_while_still_punching_is_followed_onto_the_relay() {
		let node = test_node().await;
		let public = addr("203.0.113.7:21118");
		let (peer, mut sess, _t0) = live_direct_session(&node, public).await;
		let relay_data = |seq: u64, ct: Vec<u8>| RelayMsg::RelayData {
			session: SESSION,
			payload: encode(&PeerMsg::Data { session: SESSION, seq, payload: ct }),
		};
		let proven = node.inner.lock().await.sessions[&SESSION].stamps.direct().expect("stamped at adoption");

		// The peer's background punches (unauthenticated, peer -> us only): no liveness.
		node.handle_peer(PeerMsg::Punch { session: SESSION, seq: 5 }, public).await;
		node.handle_peer(PeerMsg::PunchAck { session: SESSION, seq: 6 }, public).await;
		assert_eq!(node.inner.lock().await.sessions[&SESSION].stamps.direct(), Some(proven));

		// Relay data while the direct address delivered just now: the peer mixes paths, stay.
		node.handle_relay(relay_data(1, peer.seal(1, b"one"))).await;
		assert_eq!(sess.try_recv().as_deref(), Some(&b"one"[..]));
		assert_eq!(sess.live_transport().await, Transport::Direct);

		// The direct address quiet for over a second, relay data arrives: the peer moved — follow.
		tokio::time::sleep(Duration::from_millis(1100)).await;
		node.handle_peer(PeerMsg::Punch { session: SESSION, seq: 7 }, public).await;
		node.handle_relay(relay_data(2, peer.seal(2, b"two"))).await;
		assert_eq!(sess.try_recv().as_deref(), Some(&b"two"[..]));
		assert_eq!(sess.live_transport().await, Transport::Relay);
		{
			let g = node.inner.lock().await;
			let s = &g.sessions[&SESSION];
			assert!(!s.direct_ok);
			assert!(s.watch.fell_back_at.is_some(), "an ordinary fallback (hold-down, re-punch)");
		}

		// Forged relay data never moves a session (it doesn't decrypt).
		let node2 = test_node().await;
		let (_peer2, sess2, _t2) = live_direct_session(&node2, public).await;
		tokio::time::sleep(Duration::from_millis(1100)).await;
		node2.handle_relay(relay_data(3, vec![0u8; 40])).await;
		assert_eq!(sess2.live_transport().await, Transport::Direct);
	}
}
