//! DualSense passthrough on the session wire.
//!
//! - Input (client → host): [`Ds5Report`] inside `InputEvent::Ds5Input`.
//! - Output state (host → client), pad caps and feature reports (client → host):
//!   `DataMsg::{Ds5Output, Ds5Caps, Ds5Feature}`.
//! - Audio-rate streams ride the media-over-session channel under [`TAG_DS5`]:
//!   haptics + speaker for one 10.667 ms slot (host → client) and pad mic Opus
//!   (client → host). Lost datagrams are dropped, never repaired: late haptics are worse
//!   than missing ones.
//!
//! Everything is additive: a host advertises [`FEAT_DS5`] only when it can present a
//! virtual DualSense, and the client sends none of this to any other host.

use serde::{Deserialize, Serialize};

use super::bt_audio::{SpeakerOut, SPEAKER_OPUS_LEN};
use super::haptics::{Frame, FRAME_BYTES};
use super::report::{STATE_LEN, USB_INPUT_ID, USB_INPUT_LEN};

/// Feature id the host puts in `StreamCaps::features` when it can present a virtual
/// wired DualSense (USB/IP backend available).
pub const FEAT_DS5: &str = "ds5";

/// Media-over-session tag byte for DualSense audio-rate datagrams (`service::media`
/// uses 0x01..=0x05).
pub const TAG_DS5: u8 = 0x06;

/// One DualSense input state (the 63 bytes after the USB report id 0x01). `Copy`, so
/// `InputEvent` stays `Copy`; serialized as a hex string (126 chars) to keep the JSON
/// smaller than a number array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ds5Report(pub [u8; STATE_LEN]);

impl Ds5Report {
	/// From a full USB input report (`[0x01, state..]`).
	pub fn from_usb_input(r: &[u8; USB_INPUT_LEN]) -> Option<Self> {
		(r[0] == USB_INPUT_ID).then(|| {
			let mut s = [0u8; STATE_LEN];
			s.copy_from_slice(&r[1..]);
			Self(s)
		})
	}

	/// Back to a USB input report.
	pub fn to_usb_input(&self) -> [u8; USB_INPUT_LEN] {
		let mut r = [0u8; USB_INPUT_LEN];
		r[0] = USB_INPUT_ID;
		r[1..].copy_from_slice(&self.0);
		r
	}
}

impl Serialize for Ds5Report {
	fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
		const HEX: &[u8; 16] = b"0123456789abcdef";
		let mut out = String::with_capacity(STATE_LEN * 2);
		for b in self.0 {
			out.push(HEX[(b >> 4) as usize] as char);
			out.push(HEX[(b & 0xF) as usize] as char);
		}
		s.serialize_str(&out)
	}
}

impl<'de> Deserialize<'de> for Ds5Report {
	fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
		let b = s.as_bytes();
		if b.len() != STATE_LEN * 2 {
			return Err(serde::de::Error::custom(
				"Ds5Report: expected 126 hex chars",
			));
		}
		let nib = |c: u8| match c {
			b'0'..=b'9' => Ok(c - b'0'),
			b'a'..=b'f' => Ok(c - b'a' + 10),
			b'A'..=b'F' => Ok(c - b'A' + 10),
			_ => Err(serde::de::Error::custom("Ds5Report: bad hex")),
		};
		let mut r = [0u8; STATE_LEN];
		for (i, out) in r.iter_mut().enumerate() {
			*out = nib(b[i * 2])? << 4 | nib(b[i * 2 + 1])?;
		}
		Ok(Self(r))
	}
}

/// How the client's physical pad is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ds5Transport {
	#[default]
	Usb,
	Bt,
}

/// Media datagram kinds under [`TAG_DS5`].
const KIND_SLOT: u8 = 1;
const KIND_MIC: u8 = 2;
/// Framing version; a receiver drops other versions.
const VERSION: u8 = 1;
const HEADER: usize = 6;

/// A DualSense audio-rate datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ds5Media {
	/// Host → client: one 10.667 ms slot. On a BT pad this maps 1:1 onto a 0x32 / 0x36
	/// report; on a USB pad the client plays it on the pad's audio device.
	Slot {
		slot: u8,
		seq: u16,
		haptics: Option<Frame>,
		/// One speaker Opus frame in the pad's BT format (48 kHz stereo CELT, 200 B).
		speaker: Option<(SpeakerOut, Vec<u8>)>,
	},
	/// Client → host: one pad-mic Opus packet (48 kHz mono, 10 ms).
	Mic { slot: u8, seq: u16, opus: Vec<u8> },
}

impl Ds5Media {
	/// Encode as a session payload: `[TAG_DS5][ver][slot][kind][seq be16][body]`.
	pub fn encode(&self) -> Vec<u8> {
		let mut v = Vec::with_capacity(HEADER + FRAME_BYTES + 2 + SPEAKER_OPUS_LEN);
		v.push(TAG_DS5);
		v.push(VERSION);
		match self {
			Self::Slot {
				slot,
				seq,
				haptics,
				speaker,
			} => {
				v.push(*slot);
				v.push(KIND_SLOT);
				v.extend_from_slice(&seq.to_be_bytes());
				// flags: bit0 haptics, bit1 speaker, bit2 headphone route
				let mut flags = 0u8;
				if haptics.is_some() {
					flags |= 1;
				}
				if let Some((out, _)) = speaker {
					flags |= 2;
					if *out == SpeakerOut::Headphone {
						flags |= 4;
					}
				}
				v.push(flags);
				if let Some(h) = haptics {
					v.extend_from_slice(h);
				}
				if let Some((_, opus)) = speaker {
					v.extend_from_slice(opus);
				}
			}
			Self::Mic { slot, seq, opus } => {
				v.push(*slot);
				v.push(KIND_MIC);
				v.extend_from_slice(&seq.to_be_bytes());
				v.extend_from_slice(opus);
			}
		}
		v
	}

	/// Decode a session payload that starts with [`TAG_DS5`]. `None` for anything else.
	pub fn decode(p: &[u8]) -> Option<Self> {
		if p.len() < HEADER || p[0] != TAG_DS5 || p[1] != VERSION {
			return None;
		}
		let slot = p[2];
		let seq = u16::from_be_bytes([p[4], p[5]]);
		let body = &p[HEADER..];
		match p[3] {
			KIND_SLOT => {
				let (&flags, mut rest) = body.split_first()?;
				let haptics = if flags & 1 != 0 {
					let h: Frame = rest.get(..FRAME_BYTES)?.try_into().ok()?;
					rest = &rest[FRAME_BYTES..];
					Some(h)
				} else {
					None
				};
				let speaker = if flags & 2 != 0 {
					if rest.is_empty() {
						return None;
					}
					let out = if flags & 4 != 0 {
						SpeakerOut::Headphone
					} else {
						SpeakerOut::Speaker
					};
					Some((out, rest.to_vec()))
				} else {
					None
				};
				Some(Self::Slot {
					slot,
					seq,
					haptics,
					speaker,
				})
			}
			KIND_MIC => Some(Self::Mic {
				slot,
				seq,
				opus: body.to_vec(),
			}),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn report_hex_roundtrip() {
		let mut s = [0u8; STATE_LEN];
		for (i, b) in s.iter_mut().enumerate() {
			*b = (i * 7) as u8;
		}
		let r = Ds5Report(s);
		let json = serde_json::to_string(&r).unwrap();
		assert_eq!(json.len(), 126 + 2);
		assert!(json.starts_with("\"00070e15"));
		assert_eq!(serde_json::from_str::<Ds5Report>(&json).unwrap(), r);
		assert!(serde_json::from_str::<Ds5Report>("\"00\"").is_err());
		assert!(serde_json::from_str::<Ds5Report>(&json.replace('0', "g")).is_err());
		let usb = r.to_usb_input();
		assert_eq!(Ds5Report::from_usb_input(&usb), Some(r));
	}

	#[test]
	fn input_event_roundtrip() {
		use crate::service::InputEvent;
		let ev = InputEvent::Ds5Input {
			slot: 2,
			report: Ds5Report([0xA5; STATE_LEN]),
		};
		let json = serde_json::to_string(&ev).unwrap();
		assert!(json.starts_with(r#"{"Ds5Input":{"slot":2,"report":"a5a5"#));
		assert_eq!(serde_json::from_str::<InputEvent>(&json).unwrap(), ev);
	}

	#[test]
	fn media_roundtrip() {
		let cases = [
			Ds5Media::Slot {
				slot: 1,
				seq: 0xBEEF,
				haptics: Some([3; FRAME_BYTES]),
				speaker: None,
			},
			Ds5Media::Slot {
				slot: 0,
				seq: 7,
				haptics: Some([0x80; FRAME_BYTES]),
				speaker: Some((SpeakerOut::Headphone, vec![9; SPEAKER_OPUS_LEN])),
			},
			Ds5Media::Slot {
				slot: 2,
				seq: 8,
				haptics: None,
				speaker: Some((SpeakerOut::Speaker, vec![1, 2])),
			},
			Ds5Media::Mic {
				slot: 3,
				seq: 65535,
				opus: vec![5; 71],
			},
		];
		for c in cases {
			let e = c.encode();
			assert_eq!(e[0], TAG_DS5);
			assert_eq!(Ds5Media::decode(&e), Some(c));
		}
	}

	#[test]
	fn session_media_parser_accepts_ds5() {
		let e = Ds5Media::Mic { slot: 0, seq: 1, opus: vec![1, 2, 3] }.encode();
		let (tag, rest) = crate::service::media::parse(&e).expect("TAG_DS5 must pass media::parse");
		assert_eq!(tag, TAG_DS5);
		assert_eq!(rest.len(), e.len() - 1);
	}

	#[test]
	fn media_rejects_garbage() {
		assert_eq!(Ds5Media::decode(&[]), None);
		assert_eq!(
			Ds5Media::decode(&[0x02, 1, 0, 1, 0, 0, 0]),
			None,
			"other tag"
		);
		assert_eq!(
			Ds5Media::decode(&[TAG_DS5, 9, 0, 1, 0, 0, 0]),
			None,
			"other version"
		);
		assert_eq!(
			Ds5Media::decode(&[TAG_DS5, 1, 0, 1, 0, 0, 1, 2]),
			None,
			"short haptics"
		);
		assert_eq!(
			Ds5Media::decode(&[TAG_DS5, 1, 0, 1, 0, 0, 2]),
			None,
			"empty speaker"
		);
		assert_eq!(
			Ds5Media::decode(&[TAG_DS5, 1, 0, 9, 0, 0]),
			None,
			"unknown kind"
		);
	}
}
