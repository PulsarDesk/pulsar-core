//! Connection orchestration: the register → P2P → relay-fallback flow.
//!
//! A [`Node`] owns one UDP socket and an X25519 [`Identity`]. It:
//! 1. **Registers** with the relay to obtain its [`DeviceId`] (this is where the
//!    ID comes from — no relay, no ID).
//! 2. To reach a peer, asks the relay to rendezvous, exchanges X25519 public keys
//!    through the relay (which only sees opaque blobs), then **tries to hole-punch
//!    a direct UDP path**.
//! 3. Depending on [`NetworkMode`]: `Auto` falls back to relaying traffic if the
//!    punch fails; `P2pOnly` errors instead; `RelayOnly` skips punching entirely.
//!
//! All application data is sealed with the per-session ChaCha20-Poly1305 key, so
//! the relay (and the network) only ever see ciphertext.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use pulsar_proto::{
	decode, encode, ClientMsg, DeviceId, ErrCode, PeerMsg, PublicKey, RelayMsg, SessionId, Token,
	PROTOCOL_VERSION,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::timeout;

use crate::config::NetworkMode;
use crate::crypto::{Identity, LocalHs, PeerHs, Role, Session as Crypto};
use pulsar_proto::CAP_PAKE;

mod handlers;
mod node;
mod path;
mod portmap;
mod session;
mod types;

pub use node::{Node, RelayCreds};
pub use session::{Session, SessionSender};

use types::{Inner, SessionState};

const REGISTER_TIMEOUT: Duration = Duration::from_secs(3);
const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `connect` waits for the punch to be answered (after the opening burst) before it
/// starts the session on the relay. Punches are retransmitted every [`PUNCH_RETRY`] meanwhile,
/// and both ends keep punching in the background for [`BACKGROUND_PUNCH`] after that.
const PUNCH_TIMEOUT: Duration = Duration::from_millis(1200);
const PUNCH_RETRY: Duration = Duration::from_millis(150);
/// A session that started on the relay keeps trying hard for a direct path this long; the punch
/// handlers move it over (`transport` → `Direct`) the moment one answers. After that it keeps
/// trying every [`SLOW_PUNCH`] for as long as it stays on the relay.
const BACKGROUND_PUNCH: Duration = Duration::from_secs(20);
const SLOW_PUNCH: Duration = Duration::from_secs(15);
/// How often to ping the relay so it doesn't evict us. Must stay well under the
/// relay's `DEVICE_TTL` (30s) or `connect()` later fails with `BadToken`.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const PUNCH_ATTEMPTS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
	#[error("not registered with a relay yet")]
	NotRegistered,
	#[error("relay did not respond (is it reachable?)")]
	RelayTimeout,
	#[error("incompatible protocol version — update both ends to the same release")]
	IncompatibleVersion,
	#[error("target {0} could not be reached via the relay")]
	TargetUnreachable(DeviceId),
	#[error("direct P2P connection failed and relay fallback is disabled")]
	P2pFailed,
	/// The peer behind this id answered with a PUBLIC KEY that differs from the
	/// previously-pinned one (TOFU known-peers.json). This is a recoverable condition
	/// when the identity legitimately rotated (relay restarted + re-minted the id to a
	/// different device/key); the caller should surface a prompt and, if the user
	/// accepts, call `forget_peer` to clear the pin and retry.
	#[error("identity behind {0} changed — the pinned key no longer matches")]
	IdentityChanged(DeviceId),
	/// The relay requires authentication and this node has no (or not enough) credentials
	/// to satisfy it. The booleans say which factors are still MISSING, so the app can
	/// prompt for exactly those and retry registration with them. Not fatal — supply the
	/// credentials (a stored password and/or a fresh TOTP code) and re-register.
	#[error("relay requires authentication (password: {password}, 2FA: {totp})")]
	RelayAuthRequired { password: bool, totp: bool },
	/// A credential was supplied but the relay rejected it (wrong password or wrong TOTP).
	#[error("relay authentication failed — wrong password or 2FA code")]
	RelayAuthFailed,
	/// A password was given but the peer is an older Pulsar without the PAKE session
	/// auth. The password is NOT sent in that case (a man-in-the-middle relay could read
	/// it); the user must update the other device (or connect without a password).
	#[error("peer runs an older Pulsar without secure password auth — update it")]
	PeerOutdated,
	/// The session ended (or its state was already gone) mid-operation.
	#[error("session closed")]
	SessionClosed,
	#[error(transparent)]
	Io(#[from] std::io::Error),
}

/// How a session's media path is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
	/// Direct peer-to-peer UDP (hole-punched).
	Direct,
	/// Tunnelled through the relay.
	Relay,
}

/// Parse a relay-path handshake blob `pubkey(32) || salt(32)` into its two halves.
///
/// Returns `None` (so the caller fails the connection gracefully) if the blob is
/// too short — never index-slices an untrusted, attacker-influenced buffer.
/// `< 64` (not `!= 64`): the blob may carry the optional LAN-candidate tail
/// (see [`parse_lan_candidate`]) — anything after the fixed part is tolerated.
fn split_handshake(blob: &[u8]) -> Option<([u8; 32], PeerHs, u8)> {
	if blob.len() < 64 {
		return None;
	}
	let mut pubkey = [0u8; 32];
	let mut salt = [0u8; 32];
	pubkey.copy_from_slice(&blob[..32]);
	salt.copy_from_slice(&blob[32..64]);
	// Extension v1 (after the always-present 6-byte LAN slot): ver(1) || caps(1) || eph(32).
	let (eph, caps) = match blob.get(70..HS_BLOB_LEN) {
		Some(t) if t[0] == HS_EXT_V1 => {
			let mut eph = [0u8; 32];
			eph.copy_from_slice(&t[2..34]);
			(Some(eph), t[1])
		}
		_ => (None, 0),
	};
	Some((pubkey, PeerHs { salt, eph }, caps))
}

/// Handshake-blob extension version byte (see [`handshake_blob`]).
const HS_EXT_V1: u8 = 0x01;
/// Full v1 blob: pubkey(32) || salt(32) || LAN(6) || ver(1) || caps(1) || eph(32).
const HS_BLOB_LEN: usize = 104;
/// Capabilities this build advertises in its handshakes.
pub(crate) const OUR_CAPS: u8 = CAP_PAKE;

/// Relay-path handshake blob. The LAN slot is ALWAYS written (zeros = none, which
/// `parse_lan_candidate` reads as absent) so old peers, which read bytes 64..70 as the
/// LAN candidate and ignore anything after, keep working; new peers read the extension.
fn handshake_blob(pubkey: [u8; 32], hs: &LocalHs, lan: Option<SocketAddr>) -> Vec<u8> {
	let mut blob = Vec::with_capacity(HS_BLOB_LEN);
	blob.extend_from_slice(&pubkey);
	blob.extend_from_slice(&hs.salt);
	match lan {
		Some(SocketAddr::V4(a)) => {
			blob.extend_from_slice(&a.ip().octets());
			blob.extend_from_slice(&a.port().to_be_bytes());
		}
		_ => blob.extend_from_slice(&[0u8; 6]),
	}
	blob.push(HS_EXT_V1);
	blob.push(OUR_CAPS);
	blob.extend_from_slice(&hs.eph_public());
	blob
}

/// Optional same-LAN punch candidate appended to the 64-byte handshake blobs:
/// `v4 ip(4) || port(2)`, big-endian. Two peers behind the SAME NAT only ever
/// learn each other's PUBLIC address from the relay, so their "direct" path is a
/// router hairpin — which measurably drops a double-digit share of a 15 Mbit
/// stream on consumer gear. Punching the private candidate too lets same-LAN
/// peers go truly direct. Absent tail (old peer) → `None`, unchanged fallback.
fn parse_lan_candidate(blob: &[u8]) -> Option<SocketAddr> {
	let t = blob.get(64..70)?;
	let ip = std::net::Ipv4Addr::new(t[0], t[1], t[2], t[3]);
	let port = u16::from_be_bytes([t[4], t[5]]);
	(!ip.is_unspecified() && port != 0).then(|| SocketAddr::new(ip.into(), port))
}

/// RFC1918/link-local/loopback v4 — the "prefer the LAN path" test for punches.
fn is_private_v4(addr: SocketAddr) -> bool {
	match addr.ip() {
		std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
		_ => false,
	}
}
