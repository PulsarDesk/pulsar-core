//! Pure HID building blocks for a *virtual HID* gamepad (the macOS host backend,
//! `mac_hid.rs`, feeds these to `IOHIDUserDevice`): the report descriptors, the input
//! reports built from a [`GamepadState`], the feature reports a driver asks for at open,
//! and the rumble decoded out of the output reports a game sends.
//!
//! Two personalities, matching the other hosts' [`ResolvedTarget`]s:
//! * **Xbox** → an *Xbox Wireless Controller over Bluetooth* (045E:0B13, firmware 5.x
//!   layout). macOS has no driver for a wired Xbox 360 pad (XUSB is not HID), but the
//!   Bluetooth Xbox pad is plain HID and both Apple's GameController framework and SDL's
//!   HIDAPI driver know its 17-byte input report 0x01 and rumble output report 0x03.
//! * **DS4** → a *DualShock 4 v2 over USB* (054C:09CC): the 64-byte input report 0x01,
//!   output report 0x05 (rumble + lightbar) and the calibration / serial feature reports
//!   (0x02 / 0x12 / 0xA3) drivers read when the pad appears.
//!
//! The descriptors are written for these exact byte layouts (so a generic HID parser and a
//! VID/PID-matching driver read the same thing); the unit tests walk each descriptor and
//! check the report sizes against the builders. Everything here is platform-independent so
//! it is tested on every CI host; nothing here has been tried against a real macOS game yet.

use super::types::{button, ds4_report_fields, GamepadState};

/// Microsoft / Sony USB vendor ids.
pub const VID_MICROSOFT: u16 = 0x045E;
pub const VID_SONY: u16 = 0x054C;
/// Xbox Wireless Controller (Series X|S, Bluetooth, firmware 5.x report layout).
pub const PID_XBOX_BT: u16 = 0x0B13;
/// DualShock 4 v2 (CUH-ZCT2).
pub const PID_DS4_V2: u16 = 0x09CC;

/// Xbox (Bluetooth) input report 0x01: id + 16 bytes.
pub const XBOX_INPUT_LEN: usize = 17;
/// DS4 (USB) input report 0x01: id + 63 bytes.
pub const DS4_INPUT_LEN: usize = 64;

/// Report descriptor of the virtual Xbox Wireless (Bluetooth) pad.
///
/// Input 0x01 (17 bytes): LX LY RX RY as u16 0..65535 (centre 0x8000, Y down-positive),
/// LT RT as 10-bit values in u16 slots, hat (1..8, 0 = centred), 15 buttons + 1 pad bit
/// (A=1 B=2 X=4 Y=5 LB=7 RB=8 View=11 Menu=12 Guide=13 LS=14 RS=15 — the Bluetooth
/// numbering), Share (Consumer Record) + 7 pad bits.
/// Output 0x03 (9 bytes): enable mask, LT/RT/left/right motor (0..100), duration, delay, loops.
pub const XBOX_DESCRIPTOR: &[u8] = &[
	0x05, 0x01, // Usage Page (Generic Desktop)
	0x09, 0x05, // Usage (Game Pad)
	0xA1, 0x01, // Collection (Application)
	0x85, 0x01, //   Report ID (1)
	0x09, 0x01, //   Usage (Pointer)
	0xA1, 0x00, //   Collection (Physical)
	0x09, 0x30, //     Usage (X)
	0x09, 0x31, //     Usage (Y)
	0x15, 0x00, //     Logical Minimum (0)
	0x27, 0xFF, 0xFF, 0x00, 0x00, // Logical Maximum (65535)
	0x95, 0x02, //     Report Count (2)
	0x75, 0x10, //     Report Size (16)
	0x81, 0x02, //     Input (Data,Var,Abs)
	0xC0, //       End Collection
	0x09, 0x01, //   Usage (Pointer)
	0xA1, 0x00, //   Collection (Physical)
	0x09, 0x32, //     Usage (Z)
	0x09, 0x35, //     Usage (Rz)
	0x15, 0x00, //     Logical Minimum (0)
	0x27, 0xFF, 0xFF, 0x00, 0x00, // Logical Maximum (65535)
	0x95, 0x02, //     Report Count (2)
	0x75, 0x10, //     Report Size (16)
	0x81, 0x02, //     Input (Data,Var,Abs)
	0xC0, //       End Collection
	0x05, 0x02, //   Usage Page (Simulation Controls)
	0x09, 0xC5, //   Usage (Brake) — LT
	0x15, 0x00, //   Logical Minimum (0)
	0x26, 0xFF, 0x03, // Logical Maximum (1023)
	0x95, 0x01, //   Report Count (1)
	0x75, 0x0A, //   Report Size (10)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x00, //   Logical Maximum (0)
	0x75, 0x06, //   Report Size (6)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x03, //   Input (Const) — pad
	0x05, 0x02, //   Usage Page (Simulation Controls)
	0x09, 0xC4, //   Usage (Accelerator) — RT
	0x15, 0x00, //   Logical Minimum (0)
	0x26, 0xFF, 0x03, // Logical Maximum (1023)
	0x95, 0x01, //   Report Count (1)
	0x75, 0x0A, //   Report Size (10)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x00, //   Logical Maximum (0)
	0x75, 0x06, //   Report Size (6)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x03, //   Input (Const) — pad
	0x05, 0x01, //   Usage Page (Generic Desktop)
	0x09, 0x39, //   Usage (Hat switch)
	0x15, 0x01, //   Logical Minimum (1)
	0x25, 0x08, //   Logical Maximum (8)
	0x35, 0x00, //   Physical Minimum (0)
	0x46, 0x3B, 0x01, // Physical Maximum (315)
	0x66, 0x14, 0x00, // Unit (Degrees)
	0x75, 0x04, //   Report Size (4)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x42, //   Input (Data,Var,Abs,Null)
	0x75, 0x04, //   Report Size (4)
	0x95, 0x01, //   Report Count (1)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x00, //   Logical Maximum (0)
	0x35, 0x00, //   Physical Minimum (0)
	0x45, 0x00, //   Physical Maximum (0)
	0x65, 0x00, //   Unit (None)
	0x81, 0x03, //   Input (Const) — pad
	0x05, 0x09, //   Usage Page (Button)
	0x19, 0x01, //   Usage Minimum (1)
	0x29, 0x0F, //   Usage Maximum (15)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x01, //   Logical Maximum (1)
	0x75, 0x01, //   Report Size (1)
	0x95, 0x0F, //   Report Count (15)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x00, //   Logical Maximum (0)
	0x75, 0x01, //   Report Size (1)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x03, //   Input (Const) — pad
	0x05, 0x0C, //   Usage Page (Consumer)
	0x0A, 0xB2, 0x00, // Usage (Record) — Share
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x01, //   Logical Maximum (1)
	0x95, 0x01, //   Report Count (1)
	0x75, 0x01, //   Report Size (1)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x00, //   Logical Maximum (0)
	0x75, 0x07, //   Report Size (7)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x03, //   Input (Const) — pad
	0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
	0x09, 0x21, //   Usage (0x21) — rumble
	0x85, 0x03, //   Report ID (3)
	0x15, 0x00, //   Logical Minimum (0)
	0x26, 0xFF, 0x00, // Logical Maximum (255)
	0x75, 0x08, //   Report Size (8)
	0x95, 0x08, //   Report Count (8)
	0x91, 0x02, //   Output (Data,Var,Abs)
	0xC0, // End Collection
];

/// Report descriptor of the virtual DualShock 4 (USB) pad — the real DS4 layout for the
/// fields we drive (sticks, hat + 14 buttons + 6-bit counter, analog triggers), the rest of
/// the 64-byte report as vendor bytes (timestamp, motion, battery, touchpad), output report
/// 0x05 (32 bytes) and the feature reports drivers read on open.
pub const DS4_DESCRIPTOR: &[u8] = &[
	0x05, 0x01, // Usage Page (Generic Desktop)
	0x09, 0x05, // Usage (Game Pad)
	0xA1, 0x01, // Collection (Application)
	0x85, 0x01, //   Report ID (1)
	0x09, 0x30, //   Usage (X)
	0x09, 0x31, //   Usage (Y)
	0x09, 0x32, //   Usage (Z)
	0x09, 0x35, //   Usage (Rz)
	0x15, 0x00, //   Logical Minimum (0)
	0x26, 0xFF, 0x00, // Logical Maximum (255)
	0x75, 0x08, //   Report Size (8)
	0x95, 0x04, //   Report Count (4)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x09, 0x39, //   Usage (Hat switch)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x07, //   Logical Maximum (7)
	0x35, 0x00, //   Physical Minimum (0)
	0x46, 0x3B, 0x01, // Physical Maximum (315)
	0x65, 0x14, //   Unit (Degrees)
	0x75, 0x04, //   Report Size (4)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x42, //   Input (Data,Var,Abs,Null)
	0x65, 0x00, //   Unit (None)
	0x45, 0x00, //   Physical Maximum (0)
	0x05, 0x09, //   Usage Page (Button)
	0x19, 0x01, //   Usage Minimum (1)
	0x29, 0x0E, //   Usage Maximum (14)
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x01, //   Logical Maximum (1)
	0x75, 0x01, //   Report Size (1)
	0x95, 0x0E, //   Report Count (14)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
	0x09, 0x20, //   Usage (0x20) — report counter
	0x15, 0x00, //   Logical Minimum (0)
	0x25, 0x3F, //   Logical Maximum (63)
	0x75, 0x06, //   Report Size (6)
	0x95, 0x01, //   Report Count (1)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x05, 0x01, //   Usage Page (Generic Desktop)
	0x09, 0x33, //   Usage (Rx) — L2
	0x09, 0x34, //   Usage (Ry) — R2
	0x15, 0x00, //   Logical Minimum (0)
	0x26, 0xFF, 0x00, // Logical Maximum (255)
	0x75, 0x08, //   Report Size (8)
	0x95, 0x02, //   Report Count (2)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
	0x09, 0x21, //   Usage (0x21) — timestamp, motion, battery, touchpad
	0x95, 0x36, //   Report Count (54)
	0x81, 0x02, //   Input (Data,Var,Abs)
	0x85, 0x05, //   Report ID (5)
	0x09, 0x22, //   Usage (0x22) — rumble + lightbar
	0x95, 0x1F, //   Report Count (31)
	0x91, 0x02, //   Output (Data,Var,Abs)
	0x85, 0x02, //   Report ID (2)
	0x09, 0x24, //   Usage (0x24) — IMU calibration
	0x95, 0x24, //   Report Count (36)
	0xB1, 0x02, //   Feature (Data,Var,Abs)
	0x85, 0x12, //   Report ID (0x12)
	0x09, 0x25, //   Usage (0x25) — pairing info / MAC
	0x95, 0x0F, //   Report Count (15)
	0xB1, 0x02, //   Feature (Data,Var,Abs)
	0x85, 0xA3, //   Report ID (0xA3)
	0x09, 0x26, //   Usage (0x26) — firmware info
	0x95, 0x30, //   Report Count (48)
	0xB1, 0x02, //   Feature (Data,Var,Abs)
	0xC0, // End Collection
];

/// Hat value for the four d-pad bits, clockwise from north: 0 = N … 7 = NW, `None` when
/// centred (or the bits contradict each other).
fn hat_direction(state: &GamepadState) -> Option<u8> {
	let up = state.is_pressed(button::DPAD_UP);
	let down = state.is_pressed(button::DPAD_DOWN);
	let left = state.is_pressed(button::DPAD_LEFT);
	let right = state.is_pressed(button::DPAD_RIGHT);
	match (up, down, left, right) {
		(true, false, false, false) => Some(0),
		(true, false, false, true) => Some(1),
		(false, false, false, true) => Some(2),
		(false, true, false, true) => Some(3),
		(false, true, false, false) => Some(4),
		(false, true, true, false) => Some(5),
		(false, false, true, false) => Some(6),
		(true, false, true, false) => Some(7),
		_ => None,
	}
}

/// i16 stick (up-positive for Y once the caller negates) → u16 0..65535, centre 0x8000.
fn stick_u16(v: i16) -> u16 {
	(v as i32 + 0x8000) as u16
}

/// Build the Xbox (Bluetooth) input report 0x01 for `state`.
pub fn xbox_input_report(state: &GamepadState) -> [u8; XBOX_INPUT_LEN] {
	let mut r = [0u8; XBOX_INPUT_LEN];
	r[0] = 0x01;
	// GamepadState Y is up-positive; HID Y is down-positive.
	let axes = [
		stick_u16(state.left_x),
		stick_u16(state.left_y.saturating_neg()),
		stick_u16(state.right_x),
		stick_u16(state.right_y.saturating_neg()),
	];
	for (i, a) in axes.iter().enumerate() {
		r[1 + i * 2..3 + i * 2].copy_from_slice(&a.to_le_bytes());
	}
	let trig = |t: u8| ((t as u32 * 1023 + 127) / 255) as u16;
	r[9..11].copy_from_slice(&trig(state.left_trigger).to_le_bytes());
	r[11..13].copy_from_slice(&trig(state.right_trigger).to_le_bytes());
	r[13] = hat_direction(state).map_or(0, |h| h + 1);
	let bit = |b: u32, v: u8| if state.is_pressed(b) { v } else { 0 };
	r[14] = bit(button::A, 0x01)
		| bit(button::B, 0x02)
		| bit(button::X, 0x08)
		| bit(button::Y, 0x10)
		| bit(button::LB, 0x40)
		| bit(button::RB, 0x80);
	r[15] = bit(button::BACK, 0x04)
		| bit(button::START, 0x08)
		| bit(button::GUIDE, 0x10)
		| bit(button::L3, 0x20)
		| bit(button::R3, 0x40);
	r
}

/// Build the DS4 (USB) input report 0x01 for `state`. `counter` is the 6-bit report counter
/// (wraps), `timestamp` the 16-bit sensor clock in 5.33 µs units — drivers use both to tell
/// fresh reports from repeats.
pub fn ds4_input_report(state: &GamepadState, counter: u8, timestamp: u16) -> [u8; DS4_INPUT_LEN] {
	let mut r = [0u8; DS4_INPUT_LEN];
	let (buttons, special, lx, ly, rx, ry) = ds4_report_fields(state);
	r[0] = 0x01;
	r[1] = lx;
	r[2] = ly;
	r[3] = rx;
	r[4] = ry;
	r[5] = buttons as u8;
	r[6] = (buttons >> 8) as u8;
	r[7] = special | ((counter & 0x3F) << 2);
	r[8] = state.left_trigger;
	r[9] = state.right_trigger;
	r[10..12].copy_from_slice(&timestamp.to_le_bytes());
	// Battery: cable connected (bit 4) + full (level 11).
	r[30] = 0x1B;
	// Touchpad: one packet, both fingers lifted (bit 7 = not touching).
	r[33] = 0x01;
	r[35] = 0x80;
	r[39] = 0x80;
	r
}

/// DS4 feature report 0x02 (IMU calibration, USB layout): zero gyro bias, the ±range values
/// a stock pad reports, so a driver's calibration math stays finite.
pub fn ds4_calibration_report() -> [u8; 37] {
	let mut r = [0u8; 37];
	r[0] = 0x02;
	let vals: [i16; 14] = [
		8800, -8800, // pitch +/-
		8800, -8800, // yaw +/-
		8800, -8800, // roll +/-
		540, 540, // gyro speed +/-
		8192, -8192, // accel X +/-
		8192, -8192, // accel Y +/-
		8192, -8192, // accel Z +/-
	];
	for (i, v) in vals.iter().enumerate() {
		r[7 + i * 2..9 + i * 2].copy_from_slice(&v.to_le_bytes());
	}
	r
}

/// DS4 feature report 0x12 (pairing info): the pad's "MAC" (bytes 1..7, little-endian) —
/// drivers use it as the serial number. `unique` keeps two virtual pads apart.
pub fn ds4_pairing_report(unique: u32) -> [u8; 16] {
	let mut r = [0u8; 16];
	r[0] = 0x12;
	// Locally administered MAC 02:50:55:xx:xx:xx (stored reversed, as the pad does).
	let mac = [0x02, 0x50, 0x55, (unique >> 16) as u8, (unique >> 8) as u8, unique as u8];
	for (i, b) in mac.iter().rev().enumerate() {
		r[1 + i] = *b;
	}
	r
}

/// DS4 feature report 0xA3 (firmware info): zeros behind the id are accepted by drivers.
pub fn ds4_firmware_report() -> [u8; 49] {
	let mut r = [0u8; 49];
	r[0] = 0xA3;
	r
}

/// Strip a leading report id from an output report when it is there (`len_with_id` bytes
/// starting with `id`); some stacks hand the id over, some do not.
fn body(data: &[u8], id: u8, len_with_id: usize) -> &[u8] {
	if data.len() >= len_with_id && data.first() == Some(&id) {
		&data[1..]
	} else {
		data
	}
}

/// Decode the Xbox (Bluetooth) rumble output report 0x03 into `(large, small)` motor
/// magnitudes 0..255. `None` for any other report.
pub fn xbox_rumble(report_id: u32, data: &[u8]) -> Option<(u8, u8)> {
	if report_id != 0x03 && data.first() != Some(&0x03) {
		return None;
	}
	let b = body(data, 0x03, 9);
	// [enable, LT, RT, left (strong), right (weak), duration, delay, loops]
	if b.len() < 5 {
		return None;
	}
	let enable = b[0];
	let scale = |v: u8| ((v.min(100) as u32 * 255 + 50) / 100) as u8;
	let large = if enable & 0x02 != 0 { scale(b[3]) } else { 0 };
	let small = if enable & 0x01 != 0 { scale(b[4]) } else { 0 };
	Some((large, small))
}

/// Decode the DS4 (USB) output report 0x05 into `(large, small)` motor magnitudes 0..255.
/// `None` for another report or a lightbar-only update (flag bit 0 clear), so a colour
/// change never cancels a running rumble.
pub fn ds4_rumble(report_id: u32, data: &[u8]) -> Option<(u8, u8)> {
	if report_id != 0x05 && data.first() != Some(&0x05) {
		return None;
	}
	let b = body(data, 0x05, 32);
	// [flags, ?, ?, right (weak), left (strong), R, G, B, flash on, flash off, …]
	if b.len() < 5 || b[0] & 0x01 == 0 {
		return None;
	}
	Some((b[4], b[3]))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Per report id, the total bit length of each main-item kind (input / output / feature),
	/// from a minimal HID descriptor walk (globals: report size/count/id; push/pop unused).
	fn report_bits(desc: &[u8]) -> std::collections::BTreeMap<(u8, u8), u32> {
		let mut out = std::collections::BTreeMap::new();
		let (mut size, mut count, mut id) = (0u32, 0u32, 0u8);
		let mut i = 0;
		while i < desc.len() {
			let prefix = desc[i];
			let len = match prefix & 0x03 {
				3 => 4,
				n => n as usize,
			};
			let mut v = 0u32;
			for k in 0..len {
				v |= (desc[i + 1 + k] as u32) << (8 * k);
			}
			match prefix & 0xFC {
				0x74 => size = v,
				0x94 => count = v,
				0x84 => id = v as u8,
				0x80 => *out.entry((id, b'I')).or_insert(0) += size * count,
				0x90 => *out.entry((id, b'O')).or_insert(0) += size * count,
				0xB0 => *out.entry((id, b'F')).or_insert(0) += size * count,
				_ => {}
			}
			i += 1 + len;
		}
		out
	}

	#[test]
	fn xbox_descriptor_matches_report_sizes() {
		let bits = report_bits(XBOX_DESCRIPTOR);
		assert_eq!(bits[&(1, b'I')], (XBOX_INPUT_LEN as u32 - 1) * 8);
		assert_eq!(bits[&(3, b'O')], 8 * 8);
		assert_eq!(XBOX_DESCRIPTOR.iter().filter(|&&b| b == 0xA1).count(), 3);
	}

	#[test]
	fn ds4_descriptor_matches_report_sizes() {
		let bits = report_bits(DS4_DESCRIPTOR);
		assert_eq!(bits[&(1, b'I')], (DS4_INPUT_LEN as u32 - 1) * 8);
		assert_eq!(bits[&(5, b'O')], 31 * 8);
		assert_eq!(bits[&(2, b'F')], (ds4_calibration_report().len() as u32 - 1) * 8);
		assert_eq!(bits[&(0x12, b'F')], (ds4_pairing_report(0).len() as u32 - 1) * 8);
		assert_eq!(bits[&(0xA3, b'F')], (ds4_firmware_report().len() as u32 - 1) * 8);
	}

	#[test]
	fn xbox_report_neutral_and_pressed() {
		let mut s = GamepadState::default();
		let r = xbox_input_report(&s);
		assert_eq!(r[0], 1);
		assert_eq!(u16::from_le_bytes([r[1], r[2]]), 0x8000);
		assert_eq!(u16::from_le_bytes([r[3], r[4]]), 0x8000);
		assert_eq!(r[13], 0, "centred hat is the null value");
		assert_eq!(&r[14..], &[0, 0, 0]);

		s.set(button::A | button::Y | button::RB | button::START | button::GUIDE | button::R3, true);
		s.set(button::DPAD_UP | button::DPAD_LEFT, true);
		s.left_y = i16::MAX; // fully up
		s.left_x = i16::MIN; // fully left
		s.left_trigger = 255;
		s.right_trigger = 128;
		let r = xbox_input_report(&s);
		assert_eq!(u16::from_le_bytes([r[1], r[2]]), 0);
		assert_eq!(u16::from_le_bytes([r[3], r[4]]), 1, "up = small HID Y");
		assert_eq!(u16::from_le_bytes([r[9], r[10]]), 1023);
		assert_eq!(u16::from_le_bytes([r[11], r[12]]), 514);
		assert_eq!(r[13], 8, "NW");
		assert_eq!(r[14], 0x01 | 0x10 | 0x80);
		assert_eq!(r[15], 0x08 | 0x10 | 0x40);
	}

	#[test]
	fn ds4_report_layout() {
		let mut s = GamepadState::default();
		s.set(button::A | button::GUIDE, true);
		s.right_trigger = 77;
		let r = ds4_input_report(&s, 0x41, 0x1234);
		assert_eq!(r[0], 1);
		assert_eq!(&r[1..5], &[0x80, 0x80, 0x80, 0x80]);
		assert_eq!(r[5] & 0x0F, 8, "hat neutral");
		assert_ne!(r[5] & 0x20, 0, "cross");
		assert_eq!(r[7] & 0x03, 1, "PS");
		assert_eq!(r[7] >> 2, 0x01, "counter wraps at 6 bits");
		assert_eq!(r[9], 77);
		assert_eq!(u16::from_le_bytes([r[10], r[11]]), 0x1234);
		assert_eq!((r[35], r[39]), (0x80, 0x80), "no finger on the touchpad");
	}

	#[test]
	fn xbox_rumble_decodes_with_and_without_id() {
		let with_id = [0x03, 0x0F, 0, 0, 100, 50, 0xFF, 0, 0xEB];
		assert_eq!(xbox_rumble(3, &with_id), Some((255, 128)));
		assert_eq!(xbox_rumble(3, &with_id[1..]), Some((255, 128)));
		// Motors not enabled → silent.
		assert_eq!(xbox_rumble(3, &[0x03, 0x0C, 0, 0, 100, 100, 0xFF, 0, 0]), Some((0, 0)));
		assert_eq!(xbox_rumble(2, &[0x02, 0x0F, 0, 0, 1, 1]), None);
	}

	#[test]
	fn ds4_rumble_ignores_lightbar_only_updates() {
		let mut rep = [0u8; 32];
		rep[0] = 0x05;
		rep[1] = 0x07;
		rep[4] = 30; // weak
		rep[5] = 200; // strong
		assert_eq!(ds4_rumble(5, &rep), Some((200, 30)));
		assert_eq!(ds4_rumble(5, &rep[1..]), Some((200, 30)));
		rep[1] = 0x02; // lightbar only
		assert_eq!(ds4_rumble(5, &rep), None);
		assert_eq!(ds4_rumble(1, &[0x01, 0, 0]), None);
	}

	#[test]
	fn ds4_pairing_report_is_unique_per_pad() {
		assert_ne!(ds4_pairing_report(1), ds4_pairing_report(2));
		assert_eq!(ds4_pairing_report(7)[0], 0x12);
	}
}
