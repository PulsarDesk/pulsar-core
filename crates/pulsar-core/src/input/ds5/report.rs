//! DualSense report layouts: USB input 0x01 / output 0x02, Bluetooth input 0x31 /
//! output 0x31, and the conversions between them. Offsets follow Linux `hid-playstation`
//! and ZerkSense5 (MIT); see `docs/dualsense.md`.

use super::crc;
use super::Ds5Features;

/// Sony's USB vendor id.
pub const VID_SONY: u16 = 0x054C;
/// DualSense.
pub const PID_DUALSENSE: u16 = 0x0CE6;
/// DualSense Edge (presented as a plain DualSense in v1).
pub const PID_DUALSENSE_EDGE: u16 = 0x0DF2;

/// Input state block, shared by USB 0x01 (`[1..64]`) and BT 0x31 (`[2..65]`).
pub const STATE_LEN: usize = 63;
/// USB input report: id 0x01 + state.
pub const USB_INPUT_ID: u8 = 0x01;
pub const USB_INPUT_LEN: usize = 1 + STATE_LEN;
/// USB output report: id 0x02 + SetStateData.
pub const USB_OUTPUT_ID: u8 = 0x02;
pub const USB_OUTPUT_LEN: usize = 1 + SET_STATE_LEN;
/// Bluetooth input/output report 0x31 (78 bytes incl. id and CRC).
pub const BT_STATE_ID: u8 = 0x31;
pub const BT_STATE_LEN: usize = 78;
/// SetStateData block (47 meaningful bytes + 16 reserved).
pub const SET_STATE_LEN: usize = 63;
/// Bytes of SetStateData that carry data; the rest stays zero.
pub const SET_STATE_USED: usize = 47;

/// The neutral USB input report the virtual pad answers with before any input arrived
/// (sticks centred, hat neutral).
pub fn neutral_usb_input() -> [u8; USB_INPUT_LEN] {
	let mut r = [0u8; USB_INPUT_LEN];
	r[0] = USB_INPUT_ID;
	r[1] = 0x80;
	r[2] = 0x80;
	r[3] = 0x80;
	r[4] = 0x80;
	r[8] = 0x08; // state[7]: hat neutral, no face buttons
	r
}

/// A decoded Bluetooth input report.
#[derive(Debug, PartialEq, Eq)]
pub enum BtInput<'a> {
	/// The pad's state, already converted to the USB 0x01 layout.
	State([u8; USB_INPUT_LEN]),
	/// A mic packet (`buf[1] & 0x02`): one 71-byte Opus frame, 48 kHz mono.
	Mic(&'a [u8]),
}

/// Size of one mic Opus packet inside a BT input report.
pub const MIC_OPUS_LEN: usize = 71;

/// Decode a BT input report (`buf[0]` = report id, no HIDP header). Returns `None` for
/// anything that isn't a full 0x31 report, or when `check_crc` is set and the CRC fails.
pub fn parse_bt_input(buf: &[u8], check_crc: bool) -> Option<BtInput<'_>> {
	if buf.len() < BT_STATE_LEN || buf[0] != BT_STATE_ID {
		return None;
	}
	let buf = &buf[..BT_STATE_LEN];
	if check_crc && !crc::verify(crc::SEED_INPUT, buf) {
		return None;
	}
	if buf[1] & 0x02 != 0 {
		return Some(BtInput::Mic(&buf[3..3 + MIC_OPUS_LEN]));
	}
	let mut usb = [0u8; USB_INPUT_LEN];
	usb[0] = USB_INPUT_ID;
	usb[1..].copy_from_slice(&buf[2..2 + STATE_LEN]);
	Some(BtInput::State(usb))
}

/// The 4-bit sequence counter every BT output report carries in `buf[1]` (high nibble).
/// One counter is shared by all output report types (0x31..0x39), as the kernel and
/// ZerkSense5 do.
#[derive(Debug, Default)]
pub struct BtSeq(u8);

impl BtSeq {
	/// Stamp the next sequence number into `report[1]` and write its CRC (seed 0xA2).
	pub fn finish(&mut self, report: &mut [u8]) {
		report[1] = (report[1] & 0x0F) | (self.0 << 4);
		self.0 = (self.0 + 1) & 0x0F;
		crc::seal(crc::SEED_OUTPUT, report);
	}
}

/// SetStateData flag bits (byte 0 / byte 1 / byte 38).
pub mod flag {
	pub const B0_RUMBLE_EMULATION: u8 = 1 << 0;
	pub const B0_USE_RUMBLE_NOT_HAPTICS: u8 = 1 << 1;
	pub const B0_RIGHT_TRIGGER: u8 = 1 << 2;
	pub const B0_LEFT_TRIGGER: u8 = 1 << 3;
	pub const B0_HEADPHONE_VOLUME: u8 = 1 << 4;
	pub const B0_SPEAKER_VOLUME: u8 = 1 << 5;
	pub const B0_MIC_VOLUME: u8 = 1 << 6;
	pub const B0_AUDIO_CONTROL: u8 = 1 << 7;
	pub const B1_MUTE_LIGHT: u8 = 1 << 0;
	pub const B1_AUDIO_MUTE: u8 = 1 << 1;
	pub const B1_LED_COLOR: u8 = 1 << 2;
	pub const B1_RESET_LIGHTS: u8 = 1 << 3;
	pub const B1_PLAYER_LEDS: u8 = 1 << 4;
	pub const B1_HAPTIC_LOW_PASS: u8 = 1 << 5;
	pub const B1_MOTOR_POWER: u8 = 1 << 6;
	pub const B1_AUDIO_CONTROL2: u8 = 1 << 7;
	pub const B38_BRIGHTNESS: u8 = 1 << 0;
	pub const B38_FADE: u8 = 1 << 1;
	pub const B38_IMPROVED_RUMBLE: u8 = 1 << 2;
	pub const B38_USE_RUMBLE_NOT_HAPTICS2: u8 = 1 << 3;
}

/// Byte positions inside SetStateData.
const RUMBLE_RIGHT: usize = 2;
const RUMBLE_LEFT: usize = 3;
const AUDIO_ROUTING: usize = 7;
const NOISE_CANCEL: u8 = 1 << 3;
/// OutputPathSelect, bits 4-5 of the audio routing byte.
const OUTPUT_PATH: u8 = 0x30;
const HEADPHONE_VOLUME: usize = 4;
const SPEAKER_VOLUME: usize = 5;
const MUTE_BITS: usize = 9;
const HEADPHONE_MUTE: u8 = 1 << 6;
const AUDIO_CONTROL2: usize = 37;
/// Headphone volume used when a game enabled the field but left it at 0 (max 0x7F).
pub const DEFAULT_HEADPHONE_VOLUME: u8 = 0x50;
const RIGHT_TRIGGER: std::ops::Range<usize> = 10..21;
const LEFT_TRIGGER: std::ops::Range<usize> = 21..32;
const PLAYER_LEDS: usize = 43;
const LIGHTBAR: std::ops::Range<usize> = 44..47;

/// The game's output state (SetStateData), as carried by USB 0x02 and BT 0x31.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetState(pub [u8; SET_STATE_LEN]);

impl Default for SetState {
	fn default() -> Self {
		Self([0; SET_STATE_LEN])
	}
}

impl SetState {
	/// From a USB output report (`[0x02, data..]`). Short reports are zero-padded; the
	/// HID descriptor declares 47 data bytes, the kernel sends 63.
	pub fn from_usb_output(report: &[u8]) -> Option<Self> {
		if report.first() != Some(&USB_OUTPUT_ID) {
			return None;
		}
		Self::from_bytes(&report[1..])
	}

	/// From a bare SetStateData body (anything past 63 bytes is ignored).
	pub fn from_bytes(data: &[u8]) -> Option<Self> {
		if data.is_empty() {
			return None;
		}
		let mut s = [0u8; SET_STATE_LEN];
		let n = data.len().min(SET_STATE_LEN);
		s[..n].copy_from_slice(&data[..n]);
		Some(Self(s))
	}

	/// Drop what the client turned off, so a disabled feature never reaches the pad.
	/// Also applies the NoiseCancel quirk: with audio control on, NoiseCancel must stay
	/// set or the speaker is attenuated (DS5Dongle #263; ZerkSense5 forces it too).
	pub fn mask(&mut self, f: &Ds5Features) {
		use flag::*;
		let s = &mut self.0;
		if !f.triggers {
			s[0] &= !(B0_RIGHT_TRIGGER | B0_LEFT_TRIGGER);
			s[RIGHT_TRIGGER].fill(0);
			s[LEFT_TRIGGER].fill(0);
		}
		if !f.rumble {
			s[0] &= !B0_RUMBLE_EMULATION;
			s[RUMBLE_RIGHT] = 0;
			s[RUMBLE_LEFT] = 0;
		}
		if !f.leds {
			s[1] &= !(B1_LED_COLOR | B1_PLAYER_LEDS | B1_MUTE_LIGHT);
			s[38] &= !(B38_BRIGHTNESS | B38_FADE);
			s[PLAYER_LEDS] = 0;
			s[LIGHTBAR].fill(0);
		}
		if s[0] & B0_AUDIO_CONTROL != 0 {
			s[AUDIO_ROUTING] |= NOISE_CANCEL;
		}
	}

	/// Set classic rumble `(heavy/left, light/right)`: rumble emulation on and rumble
	/// selected over HD haptics (ZerkSense5 `RumbleEngine`).
	pub fn set_rumble(&mut self, heavy: u8, light: u8) {
		self.0[0] |= flag::B0_RUMBLE_EMULATION | flag::B0_USE_RUMBLE_NOT_HAPTICS;
		self.0[RUMBLE_LEFT] = heavy;
		self.0[RUMBLE_RIGHT] = light;
	}

	/// True if the game asked for either adaptive trigger.
	pub fn has_triggers(&self) -> bool {
		self.0[0] & (flag::B0_RIGHT_TRIGGER | flag::B0_LEFT_TRIGGER) != 0
	}

	/// Classic rumble magnitudes `(heavy/left, light/right)` when rumble emulation is on.
	pub fn rumble(&self) -> Option<(u8, u8)> {
		(self.0[0] & flag::B0_RUMBLE_EMULATION != 0)
			.then(|| (self.0[RUMBLE_LEFT], self.0[RUMBLE_RIGHT]))
	}

	/// The USB output report (`0x02` + 63 bytes).
	pub fn usb_report(&self) -> [u8; USB_OUTPUT_LEN] {
		let mut r = [0u8; USB_OUTPUT_LEN];
		r[0] = USB_OUTPUT_ID;
		r[1..].copy_from_slice(&self.0);
		r
	}

	/// The BT output report 0x31 (seq + CRC stamped by `seq`).
	pub fn bt_report(&self, seq: &mut BtSeq) -> [u8; BT_STATE_LEN] {
		let mut r = [0u8; BT_STATE_LEN];
		r[0] = BT_STATE_ID;
		r[2] = 0x10; // tag: "must be set, meaning unclear" (hid-playstation)
		r[3..3 + SET_STATE_LEN].copy_from_slice(&self.0);
		seq.finish(&mut r);
		r
	}

	/// Merge the audio routing for the headset jack into this state, the way Linux
	/// `hid-playstation` does on a plug/unplug (`dualsense_output_worker`): plugged = route
	/// L+R to the headphones and leave the internal speaker unrouted (OutputPathSelect 0);
	/// unplugged = headphones unrouted, the R channel to the speaker (path 3) at speaker
	/// volume 0x64 with +6 dB preamp. NoiseCancel stays set (the [`SetState::mask`] quirk).
	/// Other fields are left alone, so this can ride on a pending game state.
	pub fn merge_jack_route(&mut self, headphones: bool) {
		use flag::*;
		let s = &mut self.0;
		s[0] |= B0_AUDIO_CONTROL;
		// Keep MicSelect / echo cancel / input path; replace the output path.
		s[AUDIO_ROUTING] = (s[AUDIO_ROUTING] & !OUTPUT_PATH) | NOISE_CANCEL;
		if !headphones {
			s[AUDIO_ROUTING] |= 3 << 4;
			s[0] |= B0_SPEAKER_VOLUME;
			s[SPEAKER_VOLUME] = 0x64;
			s[1] |= B1_AUDIO_CONTROL2;
			s[AUDIO_CONTROL2] = (s[AUDIO_CONTROL2] & !0x07) | 0x02;
		}
	}

	/// While Pulsar plays the session's audio through the headphones: never let the host's
	/// mute (its virtual pad's speaker mute also sets HeadphoneMute, `ds5::audio::Volume`)
	/// silence the jack, and give the headphones a volume when the game set none.
	pub fn unmute_headphones(&mut self) {
		use flag::*;
		let s = &mut self.0;
		if s[1] & B1_AUDIO_MUTE != 0 {
			s[MUTE_BITS] &= !HEADPHONE_MUTE;
		}
		if s[0] & B0_HEADPHONE_VOLUME != 0 && s[HEADPHONE_VOLUME] == 0 {
			s[HEADPHONE_VOLUME] = DEFAULT_HEADPHONE_VOLUME;
		}
	}
}

/// Headset jack state from the pad's input report (USB 0x01 layout; a BT 0x31 report is
/// converted to it by [`parse_bt_input`]). Input state byte 53 (`status[1]` in
/// `hid-playstation`): bit 0 headphones (`DS_STATUS1_HP_DETECT`), bit 1 headset mic
/// (`DS_STATUS1_MIC_DETECT`). A headset with a mic sets both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Jack {
	pub headphones: bool,
	pub mic: bool,
}

/// USB input offset of the jack status byte (state offset 53 + the report id).
pub const JACK_STATUS: usize = 1 + 53;

impl Jack {
	pub fn from_usb_input(r: &[u8; USB_INPUT_LEN]) -> Self {
		let b = r[JACK_STATUS];
		Self {
			headphones: b & 0x01 != 0,
			mic: b & 0x02 != 0,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bt_input(state: &[u8; STATE_LEN], seq_tag: u8) -> [u8; BT_STATE_LEN] {
		let mut r = [0u8; BT_STATE_LEN];
		r[0] = BT_STATE_ID;
		r[1] = seq_tag;
		r[2..2 + STATE_LEN].copy_from_slice(state);
		crc::seal(crc::SEED_INPUT, &mut r);
		r
	}

	#[test]
	fn bt_input_converts_to_usb_layout() {
		let mut state = [0u8; STATE_LEN];
		state[0] = 0x11; // left X
		state[7] = 0x28; // hat 8 + cross
		state[62] = 0x99;
		let bt = bt_input(&state, 0x10);
		let Some(BtInput::State(usb)) = parse_bt_input(&bt, true) else {
			panic!("expected a state report");
		};
		assert_eq!(usb[0], USB_INPUT_ID);
		assert_eq!(&usb[1..], &state[..]);
	}

	#[test]
	fn bt_input_mic_packet_and_crc_check() {
		let mut state = [0u8; STATE_LEN];
		state[1] = 0xAB; // lands at buf[3], the first Opus byte
		let mut bt = bt_input(&state, 0x12);
		match parse_bt_input(&bt, true) {
			Some(BtInput::Mic(opus)) => {
				assert_eq!(opus.len(), MIC_OPUS_LEN);
				assert_eq!(opus[0], 0xAB);
			}
			other => panic!("expected mic, got {other:?}"),
		}
		bt[10] ^= 1;
		assert_eq!(parse_bt_input(&bt, true), None, "bad CRC rejected");
		assert!(
			parse_bt_input(&bt, false).is_some(),
			"CRC check is optional"
		);
		assert_eq!(
			parse_bt_input(&bt[..40], false),
			None,
			"short report rejected"
		);
	}

	#[test]
	fn bt_output_layout_seq_and_crc() {
		let mut s = SetState::default();
		s.0[0] = flag::B0_RIGHT_TRIGGER;
		s.0[10] = 0x21;
		let mut seq = BtSeq::default();
		let a = s.bt_report(&mut seq);
		let b = s.bt_report(&mut seq);
		assert_eq!(a[0], 0x31);
		assert_eq!(a[2], 0x10);
		assert_eq!(a[3], flag::B0_RIGHT_TRIGGER);
		assert_eq!(a[13], 0x21, "right trigger mode at report [13]");
		assert_eq!(a[1] >> 4, 0);
		assert_eq!(b[1] >> 4, 1, "seq advances per report");
		assert!(crc::verify(crc::SEED_OUTPUT, &a));
		let mut seq = BtSeq(15);
		assert_eq!(s.bt_report(&mut seq)[1] >> 4, 15);
		assert_eq!(s.bt_report(&mut seq)[1] >> 4, 0, "4-bit wrap");
	}

	#[test]
	fn usb_output_roundtrip_and_padding() {
		let short = [USB_OUTPUT_ID, 0x0C, 0x14];
		let s = SetState::from_usb_output(&short).unwrap();
		assert_eq!(s.0[0], 0x0C);
		assert_eq!(s.0[1], 0x14);
		assert!(s.0[2..].iter().all(|&b| b == 0));
		assert_eq!(s.usb_report()[..3], short);
		assert_eq!(SetState::from_usb_output(&[0x05, 1]), None);
	}

	#[test]
	fn jack_bits_from_usb_and_bt_input() {
		let mut st = [0u8; STATE_LEN];
		st[53] = 0x01;
		let BtInput::State(u) = parse_bt_input(&bt_input(&st, 0), true).unwrap() else {
			panic!("state")
		};
		assert_eq!(
			Jack::from_usb_input(&u),
			Jack {
				headphones: true,
				mic: false
			}
		);
		let mut r = neutral_usb_input();
		assert_eq!(Jack::from_usb_input(&r), Jack::default());
		r[JACK_STATUS] = 0x03 | 0x04; // headset with mic, mic muted bit ignored
		assert_eq!(
			Jack::from_usb_input(&r),
			Jack {
				headphones: true,
				mic: true
			}
		);
	}

	#[test]
	fn jack_route_matches_hid_playstation() {
		let mut s = SetState::default();
		s.0[0] = flag::B0_RIGHT_TRIGGER;
		s.0[7] = 0x01 | 0x30; // internal mic selected, speaker path
		s.merge_jack_route(true);
		assert_eq!(s.0[0], flag::B0_RIGHT_TRIGGER | flag::B0_AUDIO_CONTROL);
		assert_eq!(s.0[7], 0x01 | NOISE_CANCEL, "path 0 = L/R to headphones");
		assert_eq!(s.0[5], 0, "speaker volume untouched");
		let mut s = SetState::default();
		s.merge_jack_route(false);
		assert_eq!(s.0[7], NOISE_CANCEL | 0x30, "path 3 = R to speaker");
		assert_eq!(s.0[0], flag::B0_AUDIO_CONTROL | flag::B0_SPEAKER_VOLUME);
		assert_eq!(
			(s.0[5], s.0[1], s.0[37]),
			(0x64, flag::B1_AUDIO_CONTROL2, 0x02)
		);
	}

	#[test]
	fn headphones_stay_unmuted_while_routed() {
		let mut s = SetState::default();
		s.0[0] = flag::B0_HEADPHONE_VOLUME;
		s.0[1] = flag::B1_AUDIO_MUTE;
		s.0[9] = 0x70; // mic + speaker + headphone mute
		s.unmute_headphones();
		assert_eq!(s.0[9], 0x30, "only the headphone mute cleared");
		assert_eq!(s.0[4], DEFAULT_HEADPHONE_VOLUME);
		let mut quiet = SetState::default();
		quiet.0[9] = 0x40;
		quiet.unmute_headphones();
		assert_eq!(quiet.0[9], 0x40, "mute byte not enabled: left alone");
	}

	#[test]
	fn mask_drops_disabled_features() {
		let mut s = SetState::default();
		s.0[0] = flag::B0_RIGHT_TRIGGER
			| flag::B0_LEFT_TRIGGER
			| flag::B0_RUMBLE_EMULATION
			| flag::B0_AUDIO_CONTROL;
		s.0[1] = flag::B1_LED_COLOR | flag::B1_PLAYER_LEDS;
		s.0[2] = 50;
		s.0[3] = 200;
		s.0[10] = 0x21;
		s.0[21] = 0x25;
		s.0[43] = 0x04;
		s.0[44..47].copy_from_slice(&[255, 0, 0]);
		let all = s;

		let mut kept = all;
		kept.mask(&Ds5Features::all());
		assert_eq!(kept.0[7] & NOISE_CANCEL, NOISE_CANCEL, "noise-cancel quirk");
		assert!(kept.has_triggers());
		assert_eq!(kept.rumble(), Some((200, 50)));

		let mut off = all;
		off.mask(&Ds5Features {
			triggers: false,
			rumble: false,
			leds: false,
			..Ds5Features::all()
		});
		assert!(!off.has_triggers());
		assert!(off.0[10..32].iter().all(|&b| b == 0));
		assert_eq!(off.rumble(), None);
		assert_eq!((off.0[2], off.0[3]), (0, 0));
		assert_eq!(off.0[1] & (flag::B1_LED_COLOR | flag::B1_PLAYER_LEDS), 0);
		assert_eq!(&off.0[43..47], &[0, 0, 0, 0]);
	}
}
