//! Client-side request helpers sent over an authenticated session (list/launch
//! games, start a stream, forward input, keepalive/bye, side-channel data) plus
//! the small decoders a caller that owns the read side uses.

use super::*;

/// Client: ask the peer for its game list.
pub async fn request_games(session: &mut Session) -> Result<Vec<GameInfo>, ConnError> {
	session.send(&enc(&Msg::ListGames)).await?;
	for _ in 0..MAX_WAIT_MSGS {
		match tokio::time::timeout(
			std::time::Duration::from_secs(3),
			session.recv(),
		)
		.await
		{
			Ok(Some(bytes)) => {
				if let Some(Msg::Games(games)) = dec(&bytes) {
					return Ok(games);
				}
			}
			// None = session closed, Err(_) = per-message 3 s deadline exceeded
			Ok(None) | Err(_) => break,
		}
	}
	Ok(Vec::new())
}

/// Client: ask the host what it can actually stream (validated codec + encoder caps,
/// best-first). Empty on timeout/old host — the caller falls back to H.264. Used to
/// resolve the client's "auto" codec BEFORE writing its decoder SDP (so the SDP and
/// the stream can never disagree) and to gate the session-menu options.
pub async fn query_stream_caps(session: &mut Session) -> Result<StreamCaps, ConnError> {
	session.send(&enc(&Msg::QueryStreamCaps)).await?;
	// The query (or its answer) is one datagram: on a lossy link it can simply be lost, and
	// the empty default it falls back to means a session without NACK, FEC or the native
	// path. So ask again every CAPS_RESEND until an answer comes (the host answers each one;
	// the caller bounds the whole wait).
	const CAPS_RESEND: std::time::Duration = std::time::Duration::from_millis(400);
	const CAPS_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(3);
	let start = std::time::Instant::now();
	let mut next_send = start + CAPS_RESEND;
	let mut msgs = 0;
	while msgs < MAX_WAIT_MSGS && start.elapsed() < CAPS_GIVE_UP {
		let wait = next_send.saturating_duration_since(std::time::Instant::now());
		match tokio::time::timeout(wait, session.recv()).await {
			Ok(Some(bytes)) => {
				msgs += 1;
				if let Some(Msg::StreamCaps(caps)) = dec(&bytes) {
					return Ok(caps);
				}
			}
			// The session closed.
			Ok(None) => break,
			// No answer yet: ask again.
			Err(_) => {
				session.send(&enc(&Msg::QueryStreamCaps)).await?;
				next_send = std::time::Instant::now() + CAPS_RESEND;
			}
		}
	}
	Ok(StreamCaps::default())
}

/// Client: run the session-start bandwidth probe (`adapt::probe`) — ask the host for packet
/// trains at `rates_kbps` and measure what arrives. Call before the stream starts, only with a
/// host advertising [`media::FEAT_PROBE`]. `rtt_ms` bounds how long the tail is waited for.
/// `None` when nothing usable came back. Anything else that arrives meanwhile is dropped
/// (nothing else flows before the stream request).
pub async fn run_probe(
	session: &mut Session,
	rates_kbps: Vec<u32>,
	train_ms: u16,
	rtt_ms: f32,
) -> Result<Option<crate::adapt::probe::Estimate>, ConnError> {
	use crate::adapt::probe::{Hdr, Meter, GAP_MS};
	let trains = rates_kbps.len();
	session
		.send(&enc(&Msg::Data(DataMsg::ProbeReq { rates_kbps, train_ms })))
		.await?;
	let send_ms = trains as u64 * (train_ms as u64 + GAP_MS);
	let deadline = std::time::Instant::now()
		+ std::time::Duration::from_millis(send_ms + (2.0 * rtt_ms.clamp(0.0, 1000.0)) as u64 + 300);
	let t0 = std::time::Instant::now();
	let mut meter = Meter::new();
	while !meter.complete(trains) {
		let left = deadline.saturating_duration_since(std::time::Instant::now());
		if left.is_zero() {
			break;
		}
		match tokio::time::timeout(left, session.recv()).await {
			Ok(Some(bytes)) => {
				let arrival_ms = t0.elapsed().as_secs_f64() * 1000.0;
				if let Some((media::TAG_PROBE, rest)) = media::parse(&bytes) {
					if let Some(h) = Hdr::parse(rest) {
						meter.on_packet(&h, bytes.len(), arrival_ms);
					}
				}
			}
			Ok(None) => break,
			Err(_) => break,
		}
	}
	Ok(meter.estimate())
}

/// Client: ask the host for the list of windows it can capture per-window (Phase 2b
/// co-op "window" capture mode). Empty on timeout / old host (unknown message → no
/// reply) / non-Windows host — the caller then shows no window picker. Cheap + on
/// demand (queried when the user opens the window-capture picker), mirroring
/// [`query_stream_caps`].
pub async fn query_windows(session: &mut Session) -> Result<Vec<WindowInfo>, ConnError> {
	session.send(&enc(&Msg::QueryWindows)).await?;
	for _ in 0..MAX_WAIT_MSGS {
		match tokio::time::timeout(
			std::time::Duration::from_secs(3),
			session.recv(),
		)
		.await
		{
			Ok(Some(bytes)) => {
				if let Some(Msg::Windows(list)) = dec(&bytes) {
					return Ok(list);
				}
			}
			// None = session closed, Err(_) = per-message 3 s deadline exceeded
			Ok(None) | Err(_) => break,
		}
	}
	Ok(Vec::new())
}

/// Client: send a `QueryWindows` request WITHOUT waiting for the reply. Lets a caller
/// that owns the session's read side (the client's hold loop) fire the query and then
/// match the host's `Windows` reply out of its OWN inbound demux (via [`decode_windows`])
/// — instead of a blocking read-loop like [`query_windows`], whose 16-message reply budget
/// is otherwise consumed by the media/RTP datagrams racing on the same session (so the
/// reply almost never lands and the picker shows no windows). Mirrors [`send_data`].
pub async fn send_query_windows(session: &Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::QueryWindows)).await
}

/// Decode a received frame as the host's `Windows` reply (the answer to the request sent
/// by [`send_query_windows`]), if it is one. Lets a caller that owns the session's read
/// side resolve a pending window-list query from its inbound branch without depending on
/// the private `Msg` enum. Mirrors [`decode_data`].
pub fn decode_windows(bytes: &[u8]) -> Option<Vec<WindowInfo>> {
	match dec(bytes) {
		Some(Msg::Windows(list)) => Some(list),
		_ => None,
	}
}

/// Client: ask the peer to launch a game by id.
pub async fn request_launch(session: &mut Session, id: &str) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Launch(id.to_string()))).await
}

/// Client: ask the host to start streaming video to us.
pub async fn request_stream(session: &mut Session, req: &StreamReq) -> Result<(), ConnError> {
	session.send(&enc(&Msg::StartStream(req.clone()))).await
}

/// Client: send one control event (mouse / keyboard / controller) to the host.
pub async fn send_input(session: &mut Session, event: &InputEvent) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Input(*event))).await
}

/// Like [`send_input`] but via a cloned [`crate::SessionSender`], so a caller that
/// doesn't own the session (e.g. a mobile client's input commands, separate from the
/// read loop that owns `Session`) can forward input concurrently. `enc`/`Session::send`
/// serialize through the node lock, so this is safe alongside the read loop's sends.
pub async fn send_input_via(sender: &crate::SessionSender, event: &InputEvent) -> Result<(), ConnError> {
	sender.send(&enc(&Msg::Input(*event))).await
}

/// Client: liveness keepalive so the host's `serve` doesn't block forever after a
/// silent disconnect. Send it every ~2s while a session is held open.
pub async fn send_keepalive(session: &mut Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Ping)).await
}

/// Client: announce a graceful disconnect so the host's `serve_with` loop exits at
/// once (killing its ffmpeg + releasing held input) rather than waiting for the
/// keepalive timeout.
pub async fn send_bye(session: &mut Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Bye)).await
}

/// Like [`send_bye`] but via a cloned [`crate::SessionSender`], so a peer that only
/// holds the send half can say goodbye — specifically the HOST kicking a client (its
/// `Session` read side is owned by `serve_with`). The client's hold loop tears down
/// the instant this arrives instead of waiting out its host-silence watchdog (over a
/// relay `recv` never returns `None` when the peer vanishes).
pub async fn send_bye_via(sender: &crate::SessionSender) -> Result<(), ConnError> {
	sender.send(&enc(&Msg::Bye)).await
}

/// Either peer: send one side-channel data message (clipboard/chat/file/audio).
pub async fn send_data(session: &Session, msg: &DataMsg) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Data(msg.clone()))).await
}

/// Like [`send_data`] but via a cloned [`crate::SessionSender`], so a caller that
/// doesn't own the session (e.g. a mobile client's side-channel commands that only
/// hold a cloned sender, separate from the read loop that owns `Session`) can
/// forward data messages concurrently. Mirrors [`send_input_via`] exactly.
pub async fn send_data_via(sender: &crate::SessionSender, msg: &DataMsg) -> Result<(), ConnError> {
	sender.send(&enc(&Msg::Data(msg.clone()))).await
}

/// True if `bytes` is the host's `Pong` reply (lets the client time round-trips
/// without depending on the private `Msg` enum).
pub fn is_pong(bytes: &[u8]) -> bool {
	matches!(dec(bytes), Some(Msg::Pong))
}

/// True if `bytes` is a peer's `Bye`. Lets the client's hold loop end the instant the
/// host disconnects (the host now sends this on kick / go-offline), without depending
/// on the private `Msg` enum — over a relay `recv` never returns `None` when the peer
/// vanishes, so this is the only prompt host-gone signal.
pub fn is_bye(bytes: &[u8]) -> bool {
	matches!(dec(bytes), Some(Msg::Bye))
}

/// Decode a received frame as a side-channel [`DataMsg`], if it is one. Lets a
/// caller that owns the session's read side (e.g. the client's hold loop) pull
/// out clipboard/chat/etc. without depending on the private `Msg` enum.
pub fn decode_data(bytes: &[u8]) -> Option<DataMsg> {
	match dec(bytes) {
		Some(Msg::Data(d)) => Some(d),
		_ => None,
	}
}
