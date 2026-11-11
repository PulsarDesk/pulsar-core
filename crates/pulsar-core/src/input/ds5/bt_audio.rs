//! Bluetooth audio/haptics output reports 0x32 / 0x36 / 0x39.
//!
//! Each is a TLV container after `[id][seq<<4]`: blocks of `hdr, len, payload`, where
//! `hdr` bits 0-5 = block type, bit 6 = "two frames", bit 7 = always 1. Block types:
//! 0x11 audio control, 0x12 haptics (64 B per frame), 0x13 speaker Opus, 0x16 headphone
//! Opus (200 B per frame). The last 4 bytes are the CRC (seed 0xA2), stamped with the
//! shared sequence counter by [`BtSeq::finish`].
//!
//! Layouts are byte-for-byte those of ZerkSense5 `AudioBridge` (MIT), which match
//! DS5Dongle and SAxense (MPL-2.0; reimplemented here from the documented layout).

use super::haptics::{Frame, FRAME_BYTES};
use super::report::BtSeq;

pub const HAPTICS_ONLY_ID: u8 = 0x32;
pub const HAPTICS_ONLY_LEN: usize = 142;
pub const SINGLE_ID: u8 = 0x36;
pub const SINGLE_LEN: usize = 398;
pub const DOUBLE_ID: u8 = 0x39;
pub const DOUBLE_LEN: usize = 547;
/// One speaker Opus frame (48 kHz stereo CELT, 10 ms, CBR 160 kbit/s).
pub const SPEAKER_OPUS_LEN: usize = 200;
/// One speaker Opus frame.
pub type OpusFrame = [u8; SPEAKER_OPUS_LEN];

const BLOCK_AUDIO_CONTROL: u8 = 0x11 | 0x80;
const BLOCK_HAPTICS: u8 = 0x12 | 0x80;
const DOUBLE: u8 = 0x40;
/// Pad-side buffer length byte; ZerkSense default (valid 16..=128, unit unknown).
pub const DEFAULT_BUFFER_LEN: u8 = 48;

/// Where speaker audio plays on the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerOut {
	/// The pad's internal speaker (block 0x13).
	Speaker,
	/// The headset jack (block 0x16).
	Headphone,
}

impl SpeakerOut {
	fn block(self) -> u8 {
		0x80 | match self {
			Self::Speaker => 0x13,
			Self::Headphone => 0x16,
		}
	}
}

/// Builds the audio reports. Holds the packet counter that the 0x11 block carries
/// (advances by one per haptics frame) and the mic-streaming flag.
#[derive(Debug)]
pub struct AudioReports {
	counter: u8,
	pub buffer_len: u8,
	/// Ask the pad to stream its mic (Opus in BT input reports).
	pub mic: bool,
}

impl Default for AudioReports {
	fn default() -> Self {
		Self {
			counter: 0,
			buffer_len: DEFAULT_BUFFER_LEN,
			mic: false,
		}
	}
}

impl AudioReports {
	/// 0x32 with one or two haptics frames and no speaker audio.
	pub fn haptics_only(&mut self, frames: &[Frame], seq: &mut BtSeq) -> [u8; HAPTICS_ONLY_LEN] {
		assert!(matches!(frames.len(), 1 | 2), "one or two haptics frames");
		let double = frames.len() == 2;
		let mut p = [0u8; HAPTICS_ONLY_LEN];
		p[0] = HAPTICS_ONLY_ID;
		p[2] = BLOCK_AUDIO_CONTROL;
		p[3] = 4;
		p[4] = if self.mic { 0b0110_0111 } else { 0b0110_0110 };
		p[5] = self.buffer_len;
		p[6] = self.buffer_len;
		p[7] = self.counter;
		self.counter = self.counter.wrapping_add(frames.len() as u8);
		p[8] = BLOCK_HAPTICS | if double { DOUBLE } else { 0 };
		p[9] = FRAME_BYTES as u8;
		for (i, f) in frames.iter().enumerate() {
			p[10 + i * FRAME_BYTES..10 + (i + 1) * FRAME_BYTES].copy_from_slice(f);
		}
		seq.finish(&mut p);
		p
	}

	/// 0x36: one haptics frame + one speaker Opus frame.
	pub fn single(
		&mut self,
		haptics: &Frame,
		opus: &OpusFrame,
		out: SpeakerOut,
		seq: &mut BtSeq,
	) -> [u8; SINGLE_LEN] {
		let mut p = [0u8; SINGLE_LEN];
		p[0] = SINGLE_ID;
		p[2] = BLOCK_AUDIO_CONTROL;
		p[3] = 7;
		p[4] = if self.mic { 0xFF } else { 0xFE };
		p[5..10].fill(self.buffer_len);
		p[10] = self.counter;
		self.counter = self.counter.wrapping_add(1);
		p[11] = BLOCK_HAPTICS;
		p[12] = FRAME_BYTES as u8;
		p[13..13 + FRAME_BYTES].copy_from_slice(haptics);
		p[77] = out.block();
		p[78] = SPEAKER_OPUS_LEN as u8;
		p[79..79 + SPEAKER_OPUS_LEN].copy_from_slice(opus);
		seq.finish(&mut p);
		p
	}

	/// 0x39: two haptics frames + two speaker Opus frames.
	pub fn double(
		&mut self,
		haptics: &[Frame; 2],
		opus: &[OpusFrame; 2],
		out: SpeakerOut,
		seq: &mut BtSeq,
	) -> [u8; DOUBLE_LEN] {
		let mut p = [0u8; DOUBLE_LEN];
		p[0] = DOUBLE_ID;
		p[2] = BLOCK_AUDIO_CONTROL;
		p[3] = 6;
		p[4] = if self.mic { 0x7F } else { 0x7E };
		p[5..9].fill(self.buffer_len);
		self.counter = self.counter.wrapping_add(2);
		p[9] = self.counter;
		p[10] = BLOCK_HAPTICS | DOUBLE;
		p[11] = FRAME_BYTES as u8;
		p[12..12 + FRAME_BYTES].copy_from_slice(&haptics[0]);
		p[12 + FRAME_BYTES..12 + 2 * FRAME_BYTES].copy_from_slice(&haptics[1]);
		p[140] = out.block() | DOUBLE;
		p[141] = SPEAKER_OPUS_LEN as u8;
		p[142..142 + SPEAKER_OPUS_LEN].copy_from_slice(&opus[0]);
		p[142 + SPEAKER_OPUS_LEN..142 + 2 * SPEAKER_OPUS_LEN].copy_from_slice(&opus[1]);
		seq.finish(&mut p);
		p
	}

	/// 0x32 mic-status report: send on connect and whenever mic streaming turns on/off.
	pub fn mic_status(&self, seq: &mut BtSeq) -> [u8; HAPTICS_ONLY_LEN] {
		let mut p = [0u8; HAPTICS_ONLY_LEN];
		p[0] = HAPTICS_ONLY_ID;
		p[2] = BLOCK_AUDIO_CONTROL;
		p[3] = 1;
		p[4] = if self.mic { 0b11 } else { 0b10 };
		seq.finish(&mut p);
		p
	}

	/// 0x32 carrying a SetStateData block (type 0x10). Works like a 0x31 output report;
	/// handy for one-off changes on the audio stream's timeline.
	pub fn state(
		&self,
		state: &super::report::SetState,
		seq: &mut BtSeq,
	) -> [u8; HAPTICS_ONLY_LEN] {
		let mut p = [0u8; HAPTICS_ONLY_LEN];
		p[0] = HAPTICS_ONLY_ID;
		p[2] = 0x10 | 0x80;
		p[3] = super::report::SET_STATE_LEN as u8;
		p[4..4 + super::report::SET_STATE_LEN].copy_from_slice(&state.0);
		seq.finish(&mut p);
		p
	}
}

#[cfg(test)]
mod tests {
	use super::super::crc;
	use super::*;

	fn frame(v: u8) -> Frame {
		[v; FRAME_BYTES]
	}

	#[test]
	fn haptics_only_single_and_double() {
		let mut a = AudioReports::default();
		let mut seq = BtSeq::default();
		let r = a.haptics_only(&[frame(7)], &mut seq);
		assert_eq!(&r[..10], &[0x32, 0x00, 0x91, 4, 0x66, 48, 48, 0, 0x92, 64]);
		assert!(r[10..74].iter().all(|&b| b == 7));
		assert!(r[74..138].iter().all(|&b| b == 0));
		assert!(crc::verify(crc::SEED_OUTPUT, &r));

		a.mic = true;
		let r = a.haptics_only(&[frame(1), frame(2)], &mut seq);
		assert_eq!(r[1] >> 4, 1, "shared seq advanced");
		assert_eq!(r[4], 0x67, "mic flag");
		assert_eq!(r[7], 1, "counter = value before this report");
		assert_eq!(r[8], 0xD2, "double-frame haptics block");
		assert_eq!((r[10], r[74], r[137]), (1, 2, 2));
		let r = a.haptics_only(&[frame(0)], &mut seq);
		assert_eq!(r[7], 3, "double advanced the counter by two");
	}

	#[test]
	fn single_and_double_with_speaker() {
		let mut a = AudioReports::default();
		let mut seq = BtSeq::default();
		let opus = [0x5Au8; SPEAKER_OPUS_LEN];
		let s = a.single(&frame(9), &opus, SpeakerOut::Speaker, &mut seq);
		assert_eq!(s.len(), 398);
		assert_eq!(&s[..5], &[0x36, 0x00, 0x91, 7, 0xFE]);
		assert_eq!(s[10], 0);
		assert_eq!((s[11], s[12], s[13]), (0x92, 64, 9));
		assert_eq!((s[77], s[78], s[79], s[278]), (0x93, 200, 0x5A, 0x5A));
		assert!(s[279..394].iter().all(|&b| b == 0));
		assert!(crc::verify(crc::SEED_OUTPUT, &s));

		let d = a.double(
			&[frame(1), frame(2)],
			&[opus, opus],
			SpeakerOut::Headphone,
			&mut seq,
		);
		assert_eq!(d.len(), 547);
		assert_eq!(&d[..5], &[0x39, 0x10, 0x91, 6, 0x7E]);
		assert_eq!(d[9], 3, "1 (after single) + 2, written after the bump");
		assert_eq!((d[10], d[11], d[12], d[76]), (0xD2, 64, 1, 2));
		assert_eq!((d[140], d[141], d[142], d[541]), (0xD6, 200, 0x5A, 0x5A));
		assert_eq!(d[542], 0);
		assert!(crc::verify(crc::SEED_OUTPUT, &d));
	}

	#[test]
	fn mic_status_and_state_blocks() {
		let mut a = AudioReports::default();
		let mut seq = BtSeq::default();
		assert_eq!(&a.mic_status(&mut seq)[..5], &[0x32, 0x00, 0x91, 1, 0x02]);
		a.mic = true;
		assert_eq!(a.mic_status(&mut seq)[4], 0x03);
		let mut st = super::super::report::SetState::default();
		st.0[0] = 0xAA;
		st.0[62] = 0xBB;
		let r = a.state(&st, &mut seq);
		assert_eq!(&r[2..5], &[0x90, 63, 0xAA]);
		assert_eq!(r[66], 0xBB);
		assert!(crc::verify(crc::SEED_OUTPUT, &r));
	}
}
