//! PAKE session auth: the client proves it knows the host's one-time password (or the
//! persistent connect password) **without the password ever travelling** — not even
//! inside the encrypted session.
//!
//! Why: the session key comes from an X25519 exchange that, on a first connect, nobody
//! has verified yet (TOFU). A malicious relay could terminate that exchange with each
//! side (man-in-the-middle). With the old `Msg::Auth(password)` it would then simply read
//! the password and replay it to the real host. With SPAKE2 it can't:
//!
//! * SPAKE2 messages reveal nothing about the password; a wrong guess costs one online
//!   attempt (and the host throttles those).
//! * Both key-confirmation MACs cover the session's secret **channel binding**
//!   (`crypto::Session::binding`), which differs on the two legs of a MITM. So even a
//!   MITM that forwards the SPAKE2 messages verbatim fails confirmation on both ends.
//!
//! Flow (JSON session messages, see `Msg::AuthPake*`):
//! 1. client → host `AuthPake(Offer)`: two SPAKE2 `A` messages from the typed password —
//!    slot 0 normalised as a one-time password ([`norm_otp`]), slot 1 exact (connect
//!    password). The client can't know which one the user typed.
//! 2. host → client `AuthPakeReply(Reply)`: for each credential the host has, a SPAKE2
//!    `B` message plus a host MAC.
//! 3. client → host `AuthPakeConfirm(Confirm)`: the slot whose host MAC verified plus a
//!    client MAC (or `slot: None` = no match). The host then answers `Ok`/`NeedPassword`/
//!    `Denied` exactly as before.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpId, Password, Spake2};

use super::*;

const ID_CLIENT: &[u8] = b"pulsar-auth-client";
const ID_HOST: &[u8] = b"pulsar-auth-host";
const SLOT_OTP: u8 = 0;
const SLOT_FIXED: u8 = 1;
/// How long the host waits for the client's confirmation after its reply.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);

/// Normalise a one-time password the way it is compared: dashes dropped, lowercase. The
/// OTP is shown as `uk66-379t` (lowercase alphabet, fixed dash), so this adds no valid
/// codes. Both ends MUST use this exact function for the OTP slot.
pub fn norm_otp(s: &str) -> String {
	s.chars()
		.filter(|c| *c != '-')
		.flat_map(|c| c.to_lowercase())
		.collect()
}

/// A client's PAKE offer (opaque; check it with `AuthAttempt::verify`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
	otp: Vec<u8>,
	fixed: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplySlot {
	slot: u8,
	msg: Vec<u8>,
	mac: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Reply {
	slots: Vec<ReplySlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Confirm {
	slot: Option<u8>,
	mac: Vec<u8>,
}

/// Which host credential a client proved knowledge of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
	/// The one-time password (single-use: the host must rotate it).
	OneTime,
	/// The persistent connect password.
	Fixed,
	/// No match (wrong password, no reply, or a man-in-the-middle).
	None,
}

/// Client-side state between `AuthPake` and the host's reply (lives on the `Session`).
pub(crate) struct ClientPending {
	otp: Option<Spake2<Ed25519Group>>,
	fixed: Option<Spake2<Ed25519Group>>,
	otp_msg: Vec<u8>,
	fixed_msg: Vec<u8>,
}

fn mac(key: &[u8], side: &[u8], slot: u8, binding: &[u8; 32], a: &[u8], b: &[u8]) -> Hmac<Sha256> {
	let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
	m.update(b"pulsar-pake-v1");
	m.update(side);
	m.update(&[slot]);
	m.update(binding);
	// Length-prefix the variable fields so the transcript is unambiguous.
	m.update(&(a.len() as u32).to_be_bytes());
	m.update(a);
	m.update(&(b.len() as u32).to_be_bytes());
	m.update(b);
	m
}

fn ids() -> (SpId, SpId) {
	(SpId::new(ID_CLIENT), SpId::new(ID_HOST))
}

/// Client: a fresh attempt for `password` (two SPAKE2 `A` states, see module docs).
fn client_begin(password: &str) -> (ClientPending, Offer) {
	let (ic, ih) = ids();
	let (otp, otp_msg) =
		Spake2::<Ed25519Group>::start_a(&Password::new(norm_otp(password).as_bytes()), &ic, &ih);
	let (fixed, fixed_msg) =
		Spake2::<Ed25519Group>::start_a(&Password::new(password.as_bytes()), &ic, &ih);
	let offer = Offer {
		otp: otp_msg.clone(),
		fixed: fixed_msg.clone(),
	};
	(
		ClientPending {
			otp: Some(otp),
			fixed: Some(fixed),
			otp_msg,
			fixed_msg,
		},
		offer,
	)
}

/// Client: the confirmation for the host's reply — the first slot whose HOST MAC
/// verifies on this channel (`binding`), else `slot: None`.
fn client_confirm(mut p: ClientPending, reply: Reply, binding: &[u8; 32]) -> Confirm {
	for s in reply.slots {
		let (state, a_msg) = match s.slot {
			SLOT_OTP => (p.otp.take(), &p.otp_msg),
			SLOT_FIXED => (p.fixed.take(), &p.fixed_msg),
			_ => continue,
		};
		let Some(state) = state else { continue };
		let Ok(key) = state.finish(&s.msg) else {
			continue;
		};
		// The host must prove the same key on THIS channel before we reveal ours.
		if mac(&key, b"host", s.slot, binding, a_msg, &s.msg)
			.verify_slice(&s.mac)
			.is_err()
		{
			continue;
		}
		return Confirm {
			slot: Some(s.slot),
			mac: mac(&key, b"client", s.slot, binding, a_msg, &s.msg)
				.finalize()
				.into_bytes()
				.to_vec(),
		};
	}
	Confirm {
		slot: None,
		mac: Vec::new(),
	}
}

/// Host-side SPAKE2 result per credential slot: `(slot, key, a_msg, b_msg)`.
type HostKeys = Vec<(u8, Vec<u8>, Vec<u8>, Vec<u8>)>;

/// Host: the reply to `offer` for the credentials the host has (empty = not set).
fn host_reply(offer: Offer, otp: &str, fixed: &str, binding: &[u8; 32]) -> (Reply, HostKeys) {
	let (ic, ih) = ids();
	let otp_norm = norm_otp(otp);
	let mut keys = HostKeys::new();
	let mut slots = Vec::new();
	for (slot, pw, a_msg) in [
		(SLOT_OTP, otp_norm.as_str(), offer.otp),
		(SLOT_FIXED, fixed, offer.fixed),
	] {
		if pw.is_empty() {
			continue;
		}
		let (state, b_msg) =
			Spake2::<Ed25519Group>::start_b(&Password::new(pw.as_bytes()), &ic, &ih);
		let Ok(key) = state.finish(&a_msg) else {
			continue;
		};
		slots.push(ReplySlot {
			slot,
			msg: b_msg.clone(),
			mac: mac(&key, b"host", slot, binding, &a_msg, &b_msg)
				.finalize()
				.into_bytes()
				.to_vec(),
		});
		keys.push((slot, key, a_msg, b_msg));
	}
	(Reply { slots }, keys)
}

/// Host: which credential the client's confirmation proves on this channel.
fn host_check(keys: &HostKeys, c: &Confirm, binding: &[u8; 32]) -> Credential {
	let Some(slot) = c.slot else {
		return Credential::None;
	};
	let Some((_, key, a, b)) = keys.iter().find(|k| k.0 == slot) else {
		return Credential::None;
	};
	if mac(key, b"client", slot, binding, a, b)
		.verify_slice(&c.mac)
		.is_err()
	{
		return Credential::None;
	}
	if slot == SLOT_OTP {
		Credential::OneTime
	} else {
		Credential::Fixed
	}
}

/// Client: start a password attempt. Stores the SPAKE2 state on the session and returns
/// the `AuthPake` message to send.
pub(super) fn client_start(session: &mut Session, password: &str) -> Msg {
	let (pending, offer) = client_begin(password);
	session.pake_client = Some(pending);
	Msg::AuthPake(offer)
}

/// Client: answer the host's `AuthPakeReply` with a confirmation. A reply without a
/// pending attempt (stale/duplicate) is ignored.
pub(crate) async fn client_on_reply(session: &mut Session, reply: Reply) -> Result<(), ConnError> {
	let Some(p) = session.pake_client.take() else {
		return Ok(());
	};
	let Some(binding) = session.channel_binding().await else {
		return Err(ConnError::SessionClosed);
	};
	let confirm = client_confirm(p, reply, &binding);
	session.send(&enc(&Msg::AuthPakeConfirm(confirm))).await
}

/// Host: run the PAKE against the host's credentials (`otp` = the live one-time
/// password, `fixed` = the connect password; empty = not set) and report which one the
/// client proved. Sends the reply, then waits for the client's confirmation.
pub(crate) async fn host_verify(
	session: &mut Session,
	offer: Offer,
	otp: &str,
	fixed: &str,
) -> Credential {
	let Some(binding) = session.channel_binding().await else {
		return Credential::None;
	};
	let (reply, keys) = host_reply(offer, otp, fixed, &binding);
	if session
		.send(&enc(&Msg::AuthPakeReply(reply)))
		.await
		.is_err()
	{
		return Credential::None;
	}
	let wait = async {
		for _ in 0..MAX_WAIT_MSGS {
			match session.recv().await.as_deref().and_then(dec) {
				Some(Msg::AuthPakeConfirm(c)) => return Some(c),
				Some(_) => continue, // keepalive etc.
				None => return None,
			}
		}
		None
	};
	match timeout(CONFIRM_TIMEOUT, wait).await {
		Ok(Some(c)) => host_check(&keys, &c, &binding),
		_ => Credential::None,
	}
}

/// Constant-time string equality (legacy plaintext attempts from old clients).
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
	let (a, b) = (a.as_bytes(), b.as_bytes());
	if a.len() != b.len() {
		return false;
	}
	a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn norm_otp_drops_dash_and_case() {
		assert_eq!(norm_otp("UK66-379T"), "uk66379t");
		assert_eq!(norm_otp("uk66379t"), "uk66379t");
	}

	/// One full exchange; `cb`/`hb` = the channel binding each END sees.
	fn run(typed: &str, otp: &str, fixed: &str, cb: [u8; 32], hb: [u8; 32]) -> Credential {
		let (pending, offer) = client_begin(typed);
		let (reply, keys) = host_reply(offer, otp, fixed, &hb);
		let confirm = client_confirm(pending, reply, &cb);
		host_check(&keys, &confirm, &hb)
	}

	const B: [u8; 32] = [5u8; 32];

	#[test]
	fn otp_matches_case_and_dash_insensitively() {
		assert_eq!(run("UK66379T", "uk66-379t", "", B, B), Credential::OneTime);
		assert_eq!(
			run("uk66-379t", "uk66-379t", "sezam", B, B),
			Credential::OneTime
		);
	}

	#[test]
	fn fixed_password_matches_exactly() {
		assert_eq!(
			run("Sezam-1", "uk66-379t", "Sezam-1", B, B),
			Credential::Fixed
		);
		// The fixed password is NOT normalised: a case change must fail.
		assert_eq!(run("sezam-1", "", "Sezam-1", B, B), Credential::None);
	}

	#[test]
	fn wrong_password_or_no_credentials_fail() {
		assert_eq!(run("yanlis", "uk66-379t", "sezam", B, B), Credential::None);
		assert_eq!(run("sezam", "", "", B, B), Credential::None);
	}

	#[test]
	fn mitm_with_different_channel_binding_fails() {
		// A relay MITM terminates the X25519 exchange with each side, so the two ends
		// see different bindings. Even with the RIGHT password and the SPAKE2 messages
		// forwarded verbatim, confirmation must fail.
		assert_eq!(
			run("sezam", "", "sezam", [1u8; 32], [2u8; 32]),
			Credential::None
		);
		assert_eq!(
			run("uk66379t", "uk66-379t", "", [1u8; 32], [2u8; 32]),
			Credential::None
		);
	}

	#[test]
	fn offer_does_not_contain_the_password() {
		let (_, offer) = client_begin("sezam");
		let blob = serde_json::to_vec(&offer).unwrap();
		assert!(!blob.windows(5).any(|w| w == b"sezam"));
	}

	#[test]
	fn secret_eq_works() {
		assert!(secret_eq("abc", "abc"));
		assert!(!secret_eq("abc", "abd"));
		assert!(!secret_eq("abc", "ab"));
	}
}
