//! CRC32 used by DualSense Bluetooth reports.
//!
//! Standard CRC-32 (IEEE 802.3, reflected poly `0xEDB88320`, init `!0`, final xor `!0`)
//! computed over a one-byte **seed** followed by the report bytes, excluding the trailing
//! 4 CRC bytes. The CRC is stored little-endian in the last 4 bytes of the report.
//! The seed is the Bluetooth HID transaction header: input `0xA1`, output `0xA2`,
//! feature `0xA3` (same scheme as Linux `hid-playstation`).

/// HIDP header for DATA | INPUT.
pub const SEED_INPUT: u8 = 0xA1;
/// HIDP header for DATA | OUTPUT.
pub const SEED_OUTPUT: u8 = 0xA2;
/// HIDP header for SET/GET FEATURE.
pub const SEED_FEATURE: u8 = 0xA3;

const fn table() -> [u32; 256] {
	let mut t = [0u32; 256];
	let mut i = 0;
	while i < 256 {
		let mut c = i as u32;
		let mut k = 0;
		while k < 8 {
			c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
			k += 1;
		}
		t[i] = c;
		i += 1;
	}
	t
}

static TABLE: [u32; 256] = table();

fn update(mut crc: u32, data: &[u8]) -> u32 {
	for &b in data {
		crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
	}
	crc
}

/// Plain CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
	!update(!0, data)
}

/// CRC-32 of `seed` followed by `data`.
pub fn crc32_seeded(seed: u8, data: &[u8]) -> u32 {
	!update(update(!0, &[seed]), data)
}

/// Write the seeded CRC of `report[..len-4]` into its last 4 bytes (little-endian).
///
/// # Panics
/// If `report` is shorter than 4 bytes.
pub fn seal(seed: u8, report: &mut [u8]) {
	let n = report.len() - 4;
	let crc = crc32_seeded(seed, &report[..n]);
	report[n..].copy_from_slice(&crc.to_le_bytes());
}

/// True if the last 4 bytes of `report` hold the seeded CRC of the rest.
pub fn verify(seed: u8, report: &[u8]) -> bool {
	if report.len() < 5 {
		return false;
	}
	let n = report.len() - 4;
	let want = u32::from_le_bytes([report[n], report[n + 1], report[n + 2], report[n + 3]]);
	crc32_seeded(seed, &report[..n]) == want
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn standard_check_value() {
		// The CRC-32 catalogue check value.
		assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
	}

	#[test]
	fn seed_is_a_prefix_byte() {
		let data = [0x31u8, 0x10, 0x00, 0x7F];
		assert_eq!(crc32_seeded(SEED_OUTPUT, &data), crc32(&[0xA2, 0x31, 0x10, 0x00, 0x7F]));
	}

	#[test]
	fn seal_then_verify() {
		let mut r = [0u8; 78];
		r[0] = 0x31;
		r[1] = 0x20;
		seal(SEED_OUTPUT, &mut r);
		assert!(verify(SEED_OUTPUT, &r));
		assert!(!verify(SEED_INPUT, &r), "a different seed must not verify");
		r[5] ^= 1;
		assert!(!verify(SEED_OUTPUT, &r), "a flipped bit must not verify");
	}
}
