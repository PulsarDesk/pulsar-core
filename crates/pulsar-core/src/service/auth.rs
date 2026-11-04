//! The password auth handshake that opens a session: the client sends its access
//! request and the host returns a verdict (accept / deny / prompt).
//!
//! A non-empty password is proven with PAKE (see [`super::pake`]) and never travels.
//! An empty request (`Auth("")`, "no password yet — ask the host user") stays as is.
//! Hosts still accept a legacy plaintext `Auth(password)` from OLD clients during the
//! transition; new clients never send one (a password for an old host fails with
//! [`ConnError::PeerOutdated`] instead).

use super::pake::{self, Credential};
use super::*;

/// The host's verdict on an access request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
	/// Host approved — proceed.
	Accepted,
	/// Host rejected the session.
	Denied,
	/// A password is required; prompt the user and call `authenticate` again.
	NeedPassword,
}

/// Client: put a password attempt on the wire — `Auth("")` for "no password", a PAKE
/// offer otherwise (PAKE-capable host required).
async fn send_attempt(session: &mut Session, password: &str) -> Result<(), ConnError> {
	if password.is_empty() {
		session.pake_client = None;
		return session.send(&enc(&Msg::Auth(String::new()))).await;
	}
	if !session.peer_supports_pake().await {
		return Err(ConnError::PeerOutdated);
	}
	let msg = pake::client_start(session, password);
	session.send(&enc(&msg)).await
}

/// Client: send the access request (with the one-time password, which may be
/// empty) and wait for the host's verdict — the host may pop an Allow/Deny prompt,
/// so this can block until the host user decides. Call right after connecting,
/// before any other request.
pub async fn authenticate(session: &mut Session, password: &str) -> Result<AuthOutcome, ConnError> {
	send_attempt(session, password).await?;
	loop {
		match session.recv().await {
			Some(bytes) => match dec(&bytes) {
				Some(Msg::Ok) => return Ok(AuthOutcome::Accepted),
				Some(Msg::Denied) => return Ok(AuthOutcome::Denied),
				Some(Msg::NeedPassword) => return Ok(AuthOutcome::NeedPassword),
				Some(Msg::AuthPakeReply(r)) => pake::client_on_reply(session, r).await?,
				_ => continue,
			},
			None => return Ok(AuthOutcome::Denied),
		}
	}
}

/// A client's access attempt as the host received it.
pub enum AuthAttempt {
	/// Plaintext password from an OLD client (`""` = no password, ask the host user).
	Legacy(String),
	/// PAKE offer from a new client (the password itself is not in it).
	Pake(pake::Offer),
}

impl AuthAttempt {
	/// The "I have no password" probe (never counted as a failed attempt).
	pub fn is_empty(&self) -> bool {
		matches!(self, AuthAttempt::Legacy(p) if p.is_empty())
	}

	/// Check the attempt against the host's credentials: `otp` = the live one-time
	/// password, `fixed` = the persistent connect password (empty = not set). For a
	/// PAKE attempt this exchanges the reply/confirmation over `session`.
	///
	/// A [`Credential::OneTime`] result means the client proved THIS `otp` value — the
	/// caller must still consume it atomically (rotate only if it is still current).
	pub async fn verify(self, session: &mut Session, otp: &str, fixed: &str) -> Credential {
		match self {
			AuthAttempt::Legacy(pw) => {
				if pw.is_empty() {
					Credential::None
				} else if !otp.is_empty()
					&& pake::secret_eq(&pake::norm_otp(&pw), &pake::norm_otp(otp))
				{
					Credential::OneTime
				} else if !fixed.is_empty() && pake::secret_eq(&pw, fixed) {
					Credential::Fixed
				} else {
					Credential::None
				}
			}
			AuthAttempt::Pake(offer) => pake::host_verify(session, offer, otp, fixed).await,
		}
	}
}

fn as_attempt(m: Msg) -> Option<AuthAttempt> {
	match m {
		Msg::Auth(pw) => Some(AuthAttempt::Legacy(pw)),
		Msg::AuthPake(o) => Some(AuthAttempt::Pake(o)),
		_ => None,
	}
}

/// Host: read the client's opening access request. `None` if the peer sent something
/// else or hung up.
pub async fn recv_auth(session: &mut Session) -> Option<AuthAttempt> {
	session
		.recv()
		.await
		.as_deref()
		.and_then(dec)
		.and_then(as_attempt)
}

/// Client → host message during the auth handshake (host side).
pub enum ClientAuth {
	/// The client (re)submitted a password attempt — check it with
	/// [`AuthAttempt::verify`].
	Attempt(AuthAttempt),
	/// Keepalive or other non-auth message — ignore, keep waiting.
	Keepalive,
	/// The client went away.
	Gone,
}

/// Host: read the next message while racing the Allow/Deny popup against an
/// incoming password.
pub async fn recv_client_auth(session: &mut Session) -> ClientAuth {
	match session.recv().await.as_deref().and_then(dec) {
		Some(m) => match as_attempt(m) {
			Some(a) => ClientAuth::Attempt(a),
			None => ClientAuth::Keepalive,
		},
		None => ClientAuth::Gone,
	}
}

/// Host → client reply during the auth handshake (client side).
pub enum HostAuth {
	Ok,
	Denied,
	NeedPassword,
	Gone,
	Other,
}

/// Client: read the host's next auth reply (used to race a host approval against
/// the user typing a password). PAKE replies are answered internally.
pub async fn recv_host_auth(session: &mut Session) -> HostAuth {
	loop {
		match session.recv().await.as_deref().and_then(dec) {
			Some(Msg::Ok) => return HostAuth::Ok,
			Some(Msg::Denied) => return HostAuth::Denied,
			Some(Msg::NeedPassword) => return HostAuth::NeedPassword,
			Some(Msg::AuthPakeReply(r)) => {
				if pake::client_on_reply(session, r).await.is_err() {
					return HostAuth::Gone;
				}
			}
			None => return HostAuth::Gone,
			_ => return HostAuth::Other,
		}
	}
}

/// Client: send (or resubmit) the access password over an already-open session.
/// Fails with [`ConnError::PeerOutdated`] for a non-empty password to an old host.
pub async fn send_auth(session: &mut Session, password: &str) -> Result<(), ConnError> {
	send_attempt(session, password).await
}

/// Host: tell the client a password is required (it will prompt and retry).
pub async fn need_password(session: &mut Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::NeedPassword)).await
}

/// Host: accept the connection (after approving the request).
pub async fn accept(session: &mut Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Ok)).await
}

/// Host: reject the connection.
pub async fn reject(session: &mut Session) -> Result<(), ConnError> {
	session.send(&enc(&Msg::Denied)).await
}
