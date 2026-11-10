//! Two-factor codes for a host's optional 2FA (RFC 6238 TOTP: HMAC-SHA1, 30 s steps,
//! 6 digits — what every authenticator app produces). The host keeps the base32 secret in
//! its config; the client only ever sends the 6-digit code, inside the encrypted session.
//!
//! Same algorithm and tolerance as the relay's 2FA (`pulsar-relay` `auth.rs`), so one
//! authenticator entry format serves both.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

const STEP_SECS: u64 = 30;
const DIGITS: u32 = 6;

/// Verify `code` against the base32 `secret` at `unix_time`, accepting the current step
/// and its neighbours (±30 s) for clock skew. `false` for a malformed secret or code.
pub fn code_valid(secret_b32: &str, code: &str, unix_time: u64) -> bool {
	let Some(secret) = base32_decode(secret_b32).filter(|s| !s.is_empty()) else {
		return false;
	};
	let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
	if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
		return false;
	}
	let counter = unix_time / STEP_SECS;
	(counter.saturating_sub(1)..=counter + 1).any(|c| ct_eq(code.as_bytes(), hotp(&secret, c).as_bytes()))
}

/// [`code_valid`] against the system clock.
pub fn code_valid_now(secret_b32: &str, code: &str) -> bool {
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);
	code_valid(secret_b32, code, now)
}

/// RFC 4226 HOTP (HMAC-SHA1), zero-padded to [`DIGITS`].
fn hotp(secret: &[u8], counter: u64) -> String {
	let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
	mac.update(&counter.to_be_bytes());
	let hs = mac.finalize().into_bytes();
	let off = (hs[hs.len() - 1] & 0x0f) as usize;
	let bin = ((hs[off] as u32 & 0x7f) << 24) | ((hs[off + 1] as u32) << 16) | ((hs[off + 2] as u32) << 8) | hs[off + 3] as u32;
	format!("{:0width$}", bin % 10u32.pow(DIGITS), width = DIGITS as usize)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// RFC 4648 base32 (upper/lowercase, `=` padding, spaces and dashes ignored).
fn base32_decode(s: &str) -> Option<Vec<u8>> {
	const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
	let (mut out, mut buf, mut bits) = (Vec::new(), 0u32, 0u32);
	for ch in s.chars() {
		if matches!(ch, '=' | ' ' | '-') {
			continue;
		}
		let v = ALPHABET.iter().position(|&c| c == ch.to_ascii_uppercase() as u8)? as u32;
		buf = (buf << 5) | v;
		bits += 5;
		if bits >= 8 {
			bits -= 8;
			out.push((buf >> bits) as u8);
		}
	}
	Some(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	// RFC 6238 appendix B secret ("12345678901234567890") in base32; the RFC lists 8-digit
	// values, whose last 6 digits are the 6-digit code at the same time.
	const SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

	#[test]
	fn matches_rfc6238_vectors() {
		assert!(code_valid(SECRET, "287082", 59)); // 94287082
		assert!(code_valid(SECRET, "081804", 1_111_111_109)); // 07081804
		assert!(code_valid(SECRET, "050471", 1_111_111_111)); // 14050471
		assert!(code_valid(SECRET, "005924", 1_234_567_890)); // 89005924
		assert!(code_valid(SECRET, "279037", 2_000_000_000)); // 69279037
	}

	#[test]
	fn rejects_wrong_or_malformed_codes_and_secrets() {
		assert!(!code_valid(SECRET, "000000", 59));
		assert!(!code_valid(SECRET, "28708", 59));
		assert!(!code_valid(SECRET, "2870821", 59));
		assert!(!code_valid(SECRET, "28708a", 59));
		assert!(!code_valid("not base32!", "287082", 59));
		assert!(!code_valid("", "287082", 59));
	}

	#[test]
	fn tolerates_one_step_of_skew_and_spaces() {
		assert!(code_valid(SECRET, "287 082", 59 + 30)); // previous step still accepted
		assert!(!code_valid(SECRET, "287082", 59 + 90)); // two steps later: gone
	}
}
