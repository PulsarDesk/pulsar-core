//! The emulated wired DualSense: USB descriptors, standard / HID class / UAC1 control
//! requests, and the endpoints behind the USB/IP server.
//!
//! Ported from ZerkSense5 (MIT, florinp93), `Core/UsbDescriptors.cs` and
//! `Core/VirtualDualSense.cs`. The descriptors match ZerkSense byte for byte
//! (configuration 227 B, HID report descriptor 337 B). Protocol notes:
//! `ds5-protocol.md` §1.3, §5.1, §5.5, §6.1.
//!
//! | EP | Use |
//! |---|---|
//! | 0x01 iso OUT | 4 ch 48 kHz s16: ch 1/2 speaker, ch 3/4 haptics |
//! | 0x82 iso IN | mic, 2 ch 48 kHz s16, 192 B per 1 ms packet |
//! | 0x84 interrupt IN | input report 0x01 |
//! | 0x03 interrupt OUT | output report 0x02 |

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use super::haptics::Downsampler;
use super::report::{
	neutral_usb_input, SetState, PID_DUALSENSE, USB_INPUT_LEN, USB_OUTPUT_ID, VID_SONY,
};
use super::usbip::{trace, UsbDevice, EPIPE};
use super::virtual_pad::Ds5HostEvent;

pub const ITF_AUDIO_CONTROL: usize = 0;
pub const ITF_SPEAKER: usize = 1;
pub const ITF_MIC: usize = 2;
pub const ITF_HID: usize = 3;

/// Endpoint numbers as USB/IP carries them (no direction bit).
pub const EP_SPEAKER_OUT: u32 = 1;
pub const EP_MIC_IN: u32 = 2;
pub const EP_HID_OUT: u32 = 3;
pub const EP_HID_IN: u32 = 4;

/// UAC1 feature-unit entity ids.
pub const ENTITY_SPEAKER_FEATURE: u8 = 0x02;
pub const ENTITY_MIC_FEATURE: u8 = 0x05;

/// One 1 ms mic packet: 48 stereo s16 frames.
pub const MIC_PACKET_BYTES: usize = 48 * 2 * 2;
/// Mic FIFO cap in stereo s16 samples: 8 × 10 ms (ZerkSense `MaxQueuedFrames`).
const MIC_FIFO_MAX: usize = 8 * 480 * 2;

/// Device descriptor (18 B).
pub const DEVICE_DESCRIPTOR: [u8; 18] = [
	0x12,
	0x01,
	0x00,
	0x02,
	0x00,
	0x00,
	0x00,
	0x40, //
	(VID_SONY & 0xFF) as u8,
	(VID_SONY >> 8) as u8, //
	(PID_DUALSENSE & 0xFF) as u8,
	(PID_DUALSENSE >> 8) as u8, //
	0x00,
	0x01,
	0x01,
	0x02,
	0x00,
	0x01,
];

/// Configuration descriptor body after the 9-byte header (`UsbDescriptors.Configuration`).
#[rustfmt::skip]
const CONFIG_BODY: [u8; 218] = [
	// Interface 0: Audio Control.
	0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00,
	0x0A, 0x24, 0x01, 0x00, 0x01, 0x49, 0x00, 0x02, 0x01, 0x02,
	0x0C, 0x24, 0x02, 0x01, 0x01, 0x01, 0x06, 0x04, 0x33, 0x00, 0x00, 0x00,
	0x0C, 0x24, 0x06, 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
	0x09, 0x24, 0x03, 0x03, 0x01, 0x03, 0x04, 0x02, 0x00,
	0x0C, 0x24, 0x02, 0x04, 0x02, 0x04, 0x03, 0x02, 0x03, 0x00, 0x00, 0x00,
	0x09, 0x24, 0x06, 0x05, 0x04, 0x01, 0x03, 0x00, 0x00,
	0x09, 0x24, 0x03, 0x06, 0x01, 0x01, 0x01, 0x05, 0x00,
	// Interface 1: speaker + haptics streaming, EP 0x01 iso OUT, 392 B.
	0x09, 0x04, 0x01, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
	0x09, 0x04, 0x01, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
	0x07, 0x24, 0x01, 0x01, 0x01, 0x01, 0x00,
	0x0B, 0x24, 0x02, 0x01, 0x04, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
	0x09, 0x05, 0x01, 0x09, 0x88, 0x01, 0x01, 0x00, 0x00,
	0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
	// Interface 2: mic streaming, EP 0x82 iso IN, 196 B.
	0x09, 0x04, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
	0x09, 0x04, 0x02, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
	0x07, 0x24, 0x01, 0x06, 0x01, 0x01, 0x00,
	0x0B, 0x24, 0x02, 0x01, 0x02, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
	0x09, 0x05, 0x82, 0x05, 0xC4, 0x00, 0x01, 0x00, 0x00,
	0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
	// Interface 3: HID, EP 0x84 interrupt IN + EP 0x03 interrupt OUT, 64 B, 1 ms.
	0x09, 0x04, 0x03, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00,
	0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, (HID_REPORT_LEN & 0xFF) as u8, (HID_REPORT_LEN >> 8) as u8,
	0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x01,
	0x07, 0x05, 0x03, 0x03, 0x40, 0x00, 0x01,
];

pub const CONFIG_LEN: usize = 9 + CONFIG_BODY.len();

/// Full configuration descriptor (227 B).
pub fn config_descriptor() -> Vec<u8> {
	let mut c = vec![
		0x09,
		0x02,
		(CONFIG_LEN & 0xFF) as u8,
		(CONFIG_LEN >> 8) as u8,
		0x04,
		0x01,
		0x00,
		0xC0,
		0xFA,
	];
	c.extend_from_slice(&CONFIG_BODY);
	c
}

/// HID class descriptor (9 B).
pub fn hid_descriptor() -> [u8; 9] {
	[
		0x09,
		0x21,
		0x11,
		0x01,
		0x00,
		0x01,
		0x22,
		(HID_REPORT_LEN & 0xFF) as u8,
		(HID_REPORT_LEN >> 8) as u8,
	]
}

#[rustfmt::skip]
const HID_REPORT_INPUT: [u8; 104] = [
	0x05, 0x01, 0x09, 0x05, 0xA1, 0x01,
	0x85, 0x01,
	0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, 0x09, 0x33, 0x09, 0x34,
	0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02,
	0x06, 0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02,
	0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01,
	0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
	0x05, 0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02,
	0x06, 0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02,
	0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02,
];

/// Output report 0x02: vendor 0x23, 47 bytes.
const HID_REPORT_OUTPUT: [u8; 8] = [0x85, 0x02, 0x09, 0x23, 0x95, 0x2F, 0x91, 0x02];

/// Feature reports `(id, vendor usage, body length)`.
pub const FEATURES: [(u8, u8, u8); 28] = [
	(0x05, 0x33, 0x28),
	(0x08, 0x34, 0x2F),
	(0x09, 0x24, 0x13),
	(0x0A, 0x25, 0x1A),
	(0x0B, 0x41, 0x29),
	(0x0C, 0x42, 0x29),
	(0x20, 0x26, 0x3F),
	(0x21, 0x27, 0x04),
	(0x22, 0x40, 0x3F),
	(0x80, 0x28, 0x3F),
	(0x81, 0x29, 0x3F),
	(0x82, 0x2A, 0x09),
	(0x83, 0x2B, 0x3F),
	(0x84, 0x2C, 0x3F),
	(0x85, 0x2D, 0x02),
	(0xA0, 0x2E, 0x01),
	(0xE0, 0x2F, 0x3F),
	(0xF0, 0x30, 0x3F),
	(0xF1, 0x31, 0x3F),
	(0xF2, 0x32, 0x0F),
	(0xF4, 0x35, 0x3F),
	(0xF5, 0x36, 0x03),
	(0xF6, 0x37, 0x3F),
	(0xF7, 0x38, 0x3F),
	(0xF8, 0x39, 0x3F),
	(0xF9, 0x3A, 0x3F),
	(0xFA, 0x3B, 0x3F),
	(0xFB, 0x3C, 0x3F),
];

pub const HID_REPORT_LEN: usize =
	HID_REPORT_INPUT.len() + HID_REPORT_OUTPUT.len() + FEATURES.len() * 8 + 1;

/// HID report descriptor (337 B).
pub fn hid_report_descriptor() -> Vec<u8> {
	let mut d = Vec::with_capacity(HID_REPORT_LEN);
	d.extend_from_slice(&HID_REPORT_INPUT);
	d.extend_from_slice(&HID_REPORT_OUTPUT);
	for (id, usage, count) in FEATURES {
		d.extend_from_slice(&[0x85, id, 0x09, usage, 0x95, count, 0xB1, 0x02]);
	}
	d.push(0xC0);
	d
}

/// Body length (without the id) of feature report `id`, if the descriptor declares it.
pub fn feature_len(id: u8) -> Option<usize> {
	FEATURES.iter().find(|f| f.0 == id).map(|f| f.2 as usize)
}

/// String descriptor `index`, or `None` (STALL).
pub fn string_descriptor(index: u8) -> Option<Vec<u8>> {
	let s = match index {
		0 => return Some(vec![0x04, 0x03, 0x09, 0x04]),
		1 => "Sony Interactive Entertainment",
		2 => "DualSense Wireless Controller",
		_ => return None,
	};
	let mut d = vec![0, 0x03];
	for u in s.encode_utf16() {
		d.extend_from_slice(&u.to_le_bytes());
	}
	d[0] = d.len() as u8;
	Some(d)
}

/// Canned firmware-info body (feature 0x20 without the id) of a plain DualSense
/// (`FakeDs5.Report20`, ZerkSense5 `DualSenseBt.cs:217-226`; DS5Dongle `fake_ds5.h`).
pub const FAKE_FIRMWARE_0X20: [u8; 63] = [
	0x4a, 0x75, 0x6c, 0x20, 0x20, 0x34, 0x20, 0x32, 0x30, 0x32, 0x35, 0x31, 0x30, 0x3a, 0x33, 0x38,
	0x3a, 0x34, 0x30, 0x03, 0x00, 0x0b, 0x00, 0x11, 0x08, 0x00, 0x00, 0x2a, 0x00, 0x10, 0x01, 0x00,
	0x28, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x06, 0x00, 0x00, 0x01,
	0x00, 0x03, 0x00, 0x10, 0x10, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Synthetic IMU calibration body (feature 0x05 without the id), used until a live pad
/// supplies its own.
///
/// An all-zero report is survivable: `hid-playstation` (since 5.17) sees a zero
/// denominator, warns "Invalid gyro calibration data" and falls back to raw ranges, and
/// SDL's PS5 driver likewise drops calibration. But a zero bias with ±8640 gyro / ±8192
/// accel references is what a real pad roughly reports, and it maps raw units 1:1 to the
/// physical scale both drivers expect, so we serve that instead and avoid the warning:
///
/// - gyro: bias 0, pitch/yaw/roll plus +8640 / minus −8640, speed plus = minus = 540.
///   Kernel sensitivity = `(540+540)·1024 / (8640+8640)` = 64 per raw unit, i.e. raw / 16
///   °/s, the DualSense's native ±2000 °/s scale.
/// - accel: x/y/z plus +8192 / minus −8192 → bias 0, `2·8192 / 16384` = 1:1, 8192 = 1 g.
///
/// Layout (offsets without the id): bias pitch/yaw/roll @0,2,4; pitch± @6,8; yaw± @10,12;
/// roll± @14,16; speed± @18,20; accel x± @22,24; y± @26,28; z± @30,32 (LE16).
pub fn default_calibration() -> [u8; 0x28] {
	let mut b = [0u8; 0x28];
	let mut put = |o: usize, v: i16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
	for o in [6, 10, 14] {
		put(o, 8640);
		put(o + 2, -8640);
	}
	put(18, 540);
	put(20, 540);
	for o in [22, 26, 30] {
		put(o, 8192);
		put(o + 2, -8192);
	}
	b
}

/// Pairing-info body (feature 0x09 without the id) for `mac` (display order, most
/// significant octet first). The report stores it little-endian (kernel
/// `memcpy(mac, &buf[1], 6)` then prints it reversed).
pub fn pairing_info(mac: [u8; 6]) -> [u8; 0x13] {
	let mut b = [0u8; 0x13];
	for i in 0..6 {
		b[i] = mac[5 - i];
	}
	b
}

/// Speaker volume the virtual pad reports before the OS sets one: -20 dB. A new USB audio
/// device otherwise starts at 0 dB, i.e. the pad speaker at full blast (maintainer test).
const DEFAULT_SPEAKER_DB: i16 = -20;

#[derive(Debug)]
struct State {
	alt: [u8; 4],
	/// UAC mute (speaker, mic).
	mute: [u8; 2],
	/// UAC volume in 1/256 dB (speaker, mic).
	volume: [i16; 2],
	last_input: [u8; USB_INPUT_LEN],
	features: HashMap<u8, Vec<u8>>,
	haptics: Downsampler,
	mic: VecDeque<i16>,
	/// Mic samples to queue before iso IN starts taking them, and again after the queue ran
	/// dry (0 = none: the local bridge's pad mic arrives evenly; a session's does not).
	mic_prebuffer: usize,
	mic_primed: bool,
}

/// The emulated DualSense the USB/IP server exposes.
pub struct Ds5Device {
	state: Mutex<State>,
	events: Mutex<Sender<Ds5HostEvent>>,
}

const INTERFACES: [(u8, u8, u8); 4] = [(1, 1, 0), (1, 2, 0), (1, 2, 0), (3, 0, 0)];

impl Ds5Device {
	pub fn new(mac: [u8; 6], haptics_gain: f32, events: Sender<Ds5HostEvent>) -> Self {
		let mut features: HashMap<u8, Vec<u8>> = FEATURES
			.iter()
			.map(|&(id, _, n)| (id, vec![0u8; n as usize]))
			.collect();
		features.insert(0x05, default_calibration().to_vec());
		features.insert(0x09, pairing_info(mac).to_vec());
		features.insert(0x20, FAKE_FIRMWARE_0X20.to_vec());
		Self {
			state: Mutex::new(State {
				alt: [0; 4],
				mute: [0; 2],
				volume: [DEFAULT_SPEAKER_DB * 256, 48 * 256],
				last_input: neutral_usb_input(),
				features,
				haptics: Downsampler::new(haptics_gain),
				mic: VecDeque::new(),
				mic_prebuffer: 0,
				mic_primed: false,
			}),
			events: Mutex::new(events),
		}
	}

	fn emit(&self, e: Ds5HostEvent) {
		let _ = self.events.lock().unwrap().send(e);
	}

	/// The latest input report (also answered to GET_REPORT(Input)).
	pub fn set_input(&self, report: &[u8; USB_INPUT_LEN]) {
		self.state.lock().unwrap().last_input = *report;
	}

	/// Replace a cached feature report with a live pad's. `data` is the body without the
	/// id; a buffer that still starts with the id at full length is accepted too. Short
	/// data is zero-padded, long data cut to the descriptor size. A DualSense Edge
	/// firmware report (0x20 with body[21] == 0x44) is ignored, since the virtual pad is a
	/// plain DualSense. Ids outside the descriptor (Edge profile slots) are stored as is.
	pub fn set_feature(&self, id: u8, data: &[u8]) {
		let want = feature_len(id);
		let body = match want {
			Some(n) if data.len() == n + 1 && data[0] == id => &data[1..],
			_ => data,
		};
		if id == 0x20 && body.get(21) == Some(&0x44) {
			return;
		}
		let mut v = body.to_vec();
		if let Some(n) = want {
			v.resize(n, 0);
		}
		self.state.lock().unwrap().features.insert(id, v);
	}

	/// Hold back iso IN until `ms` of mic audio is queued (a jitter buffer for a mic that
	/// comes over the network), and again after every underrun. 0 turns it off.
	pub fn set_mic_prebuffer(&self, ms: u32) {
		let mut st = self.state.lock().unwrap();
		st.mic_prebuffer = (ms as usize * 48 * 2).min(MIC_FIFO_MAX / 2);
		st.mic_primed = false;
	}

	/// Queue client mic audio: 48 kHz interleaved stereo s16. Oldest samples are dropped
	/// past 80 ms of backlog.
	pub fn push_mic(&self, pcm: &[i16]) {
		let mut st = self.state.lock().unwrap();
		if st.alt[ITF_MIC] == 0 {
			return;
		}
		st.mic.extend(pcm.iter().copied());
		let over = st.mic.len().saturating_sub(MIC_FIFO_MAX);
		let over = over + (over & 1);
		let n = over.min(st.mic.len());
		st.mic.drain(..n);
	}

	fn volume_event(st: &State) -> Ds5HostEvent {
		Ds5HostEvent::Volume {
			speaker_db: st.volume[0] / 256,
			mic_db: st.volume[1] / 256,
			speaker_mute: st.mute[0] != 0,
			mic_mute: st.mute[1] != 0,
		}
	}

	fn output(&self, body: &[u8]) {
		if let Some(s) = SetState::from_bytes(body) {
			self.emit(Ds5HostEvent::Output(s));
		}
	}

	fn standard(&self, req: u8, value: u16, index: u16, recipient: u8) -> Result<Vec<u8>, i32> {
		match req {
			// GET_STATUS
			0x00 => Ok(if recipient == 0 {
				vec![1, 0]
			} else {
				vec![0, 0]
			}),
			// CLEAR_FEATURE, SET_FEATURE, SET_ADDRESS, SET_CONFIGURATION
			0x01 | 0x03 | 0x05 | 0x09 => Ok(vec![]),
			// GET_CONFIGURATION
			0x08 => Ok(vec![1]),
			// GET_INTERFACE
			0x0A => {
				let st = self.state.lock().unwrap();
				Ok(vec![st.alt[(index as usize & 0xFF).min(3)]])
			}
			// SET_INTERFACE
			0x0B => {
				let (itf, alt) = (index as usize & 0xFF, value as u8);
				if itf > 3 {
					return Err(-EPIPE);
				}
				let mut st = self.state.lock().unwrap();
				let was = st.alt[itf];
				st.alt[itf] = alt;
				trace!("SET_INTERFACE interface {itf} alt {was} -> {alt}");
				let vol = Self::volume_event(&st);
				if itf == ITF_MIC && alt == 0 {
					st.mic.clear();
					st.mic_primed = false;
				}
				drop(st);
				if itf == ITF_SPEAKER && (was != 0) != (alt != 0) {
					tracing::info!(
						"ds5: speaker/haptics stream {}",
						if alt != 0 { "opened" } else { "closed" }
					);
					self.emit(Ds5HostEvent::SpeakerOpen(alt != 0));
					// The pad stays silent unless volume/mute is sent on open
					// (ZerkSense `SendSpeakerLevel`).
					if alt != 0 {
						self.emit(vol);
					}
				}
				if itf == ITF_MIC && (was != 0) != (alt != 0) {
					tracing::info!(
						"ds5: mic stream {}",
						if alt != 0 { "opened" } else { "closed" }
					);
					self.emit(Ds5HostEvent::MicOpen(alt != 0));
				}
				Ok(vec![])
			}
			// GET_DESCRIPTOR
			0x06 => {
				let (ty, idx) = ((value >> 8) as u8, value as u8);
				if recipient == 1 {
					return match ty {
						0x22 => Ok(hid_report_descriptor()),
						0x21 => Ok(hid_descriptor().to_vec()),
						_ => Err(-EPIPE),
					};
				}
				match ty {
					0x01 => Ok(DEVICE_DESCRIPTOR.to_vec()),
					0x02 => Ok(config_descriptor()),
					0x03 => string_descriptor(idx).ok_or(-EPIPE),
					_ => Err(-EPIPE),
				}
			}
			_ => Err(-EPIPE),
		}
	}

	fn hid_class(&self, req: u8, value: u16, out: &[u8]) -> Result<Vec<u8>, i32> {
		let (ty, id) = ((value >> 8) as u8, value as u8);
		match req {
			// GET_REPORT
			0x01 => {
				let st = self.state.lock().unwrap();
				match ty {
					1 => Ok(st.last_input.to_vec()),
					3 => {
						let body = st.features.get(&id).ok_or(-EPIPE)?;
						let mut r = Vec::with_capacity(1 + body.len());
						r.push(id);
						r.extend_from_slice(body);
						Ok(r)
					}
					_ => Err(-EPIPE),
				}
			}
			// SET_REPORT: a non-zero id is repeated in data[0].
			0x09 => {
				if out.is_empty() {
					return Ok(vec![]);
				}
				let body = if id != 0 { &out[1..] } else { out };
				if ty == 2 && id == USB_OUTPUT_ID {
					self.output(body);
				} else if ty == 3 && (id == 0x80 || (0x60..=0x62).contains(&id)) {
					self.emit(Ds5HostEvent::SetFeature {
						id,
						data: body.to_vec(),
					});
				}
				Ok(vec![])
			}
			// SET_IDLE, SET_PROTOCOL
			0x0A | 0x0B => Ok(vec![]),
			// GET_IDLE
			0x02 => Ok(vec![0]),
			// GET_PROTOCOL
			0x03 => Ok(vec![1]),
			_ => Err(-EPIPE),
		}
	}

	fn audio_class(&self, req: u8, value: u16, index: u16, out: &[u8]) -> Result<Vec<u8>, i32> {
		let control = value >> 8;
		let entity = (index >> 8) as u8;
		let i = match entity {
			ENTITY_SPEAKER_FEATURE => 0,
			ENTITY_MIC_FEATURE => 1,
			_ => return Err(-EPIPE),
		};
		let spk = i == 0;
		let mut st = self.state.lock().unwrap();
		if req == 0x01 {
			// SET_CUR
			match control {
				1 if !out.is_empty() => st.mute[i] = out[0],
				2 if out.len() >= 2 => st.volume[i] = i16::from_le_bytes([out[0], out[1]]),
				_ => return Err(-EPIPE),
			}
			let e = Self::volume_event(&st);
			drop(st);
			self.emit(e);
			return Ok(vec![]);
		}
		if control == 1 {
			return Ok(vec![st.mute[i]]);
		}
		if control != 2 {
			return Err(-EPIPE);
		}
		let v: i16 = match req {
			0x81 => st.volume[i],
			0x82 => {
				if spk {
					-100 * 256
				} else {
					0
				}
			}
			0x83 => {
				if spk {
					0
				} else {
					0x3000
				}
			}
			0x84 => {
				if spk {
					0x0100
				} else {
					0x007A
				}
			}
			_ => return Err(-EPIPE),
		};
		Ok(v.to_le_bytes().to_vec())
	}
}

impl UsbDevice for Ds5Device {
	fn vendor_id(&self) -> u16 {
		VID_SONY
	}
	fn product_id(&self) -> u16 {
		PID_DUALSENSE
	}
	fn bcd_device(&self) -> u16 {
		0x0100
	}
	fn interfaces(&self) -> &[(u8, u8, u8)] {
		&INTERFACES
	}

	fn control(&self, setup: &[u8; 8], out: &[u8]) -> Result<Vec<u8>, i32> {
		let ty = setup[0];
		let req = setup[1];
		let value = u16::from_le_bytes([setup[2], setup[3]]);
		let index = u16::from_le_bytes([setup[4], setup[5]]);
		let kind = (ty >> 5) & 3;
		let recipient = ty & 0x1F;
		match (kind, recipient) {
			(0, _) => self.standard(req, value, index, recipient),
			(1, 1) => match index as usize & 0xFF {
				ITF_HID => self.hid_class(req, value, out),
				ITF_AUDIO_CONTROL => self.audio_class(req, value, index, out),
				_ => Err(-EPIPE),
			},
			// Endpoint sampling frequency: always 48 kHz.
			(1, 2) => Ok(if ty & 0x80 != 0 {
				vec![0x80, 0xBB, 0x00]
			} else {
				vec![]
			}),
			_ => Err(-EPIPE),
		}
	}

	fn interrupt_out(&self, ep: u32, data: &[u8]) {
		if ep == EP_HID_OUT && data.first() == Some(&USB_OUTPUT_ID) {
			self.output(&data[1..]);
		}
	}

	fn iso_out(&self, ep: u32, packets: &[&[u8]]) {
		if ep != EP_SPEAKER_OUT {
			return;
		}
		let mut st = self.state.lock().unwrap();
		if st.alt[ITF_SPEAKER] == 0 {
			return;
		}
		let frames: usize = packets.iter().map(|p| p.len() / 8).sum();
		let mut pcm = Vec::with_capacity(frames * 4);
		let mut speaker = Vec::with_capacity(frames * 2);
		for p in packets {
			for f in p.chunks_exact(8) {
				let s = |k: usize| i16::from_le_bytes([f[2 * k], f[2 * k + 1]]);
				speaker.push(s(0) as f32 / 32768.0);
				speaker.push(s(1) as f32 / 32768.0);
				pcm.extend_from_slice(&[s(0), s(1), s(2), s(3)]);
			}
		}
		let mut frames_out = Vec::new();
		st.haptics.push_usb_s16(&pcm, &mut frames_out);
		drop(st);
		if !speaker.is_empty() {
			self.emit(Ds5HostEvent::Speaker(speaker));
		}
		for f in frames_out {
			self.emit(Ds5HostEvent::Haptics(f));
		}
	}

	fn iso_in(&self, _ep: u32, packet: &mut [u8]) {
		let mut st = self.state.lock().unwrap();
		let muted = st.mute[1] != 0;
		if !st.mic_primed {
			if st.mic.len() < st.mic_prebuffer.max(1) {
				packet.fill(0);
				return;
			}
			st.mic_primed = true;
		}
		for s in packet.chunks_exact_mut(2) {
			let v = st.mic.pop_front().unwrap_or(0);
			s.copy_from_slice(&if muted { 0i16 } else { v }.to_le_bytes());
		}
		if st.mic_prebuffer > 0 && st.mic.is_empty() {
			// Ran dry: prebuffer again before the next packet.
			st.mic_primed = false;
		}
	}

	fn is_isochronous(&self, ep: u32) -> bool {
		ep == EP_SPEAKER_OUT || ep == EP_MIC_IN
	}

	fn iso_in_packet_size(&self, _ep: u32) -> usize {
		MIC_PACKET_BYTES
	}

	fn session_changed(&self, attached: bool) {
		if attached {
			return;
		}
		let mut st = self.state.lock().unwrap();
		let (spk, mic) = (st.alt[ITF_SPEAKER] != 0, st.alt[ITF_MIC] != 0);
		st.alt = [0; 4];
		st.mic.clear();
		st.mic_primed = false;
		drop(st);
		if spk {
			self.emit(Ds5HostEvent::SpeakerOpen(false));
		}
		if mic {
			self.emit(Ds5HostEvent::MicOpen(false));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn descriptor_lengths_and_key_bytes() {
		let c = config_descriptor();
		assert_eq!(c.len(), 227);
		assert_eq!(CONFIG_LEN, 227);
		assert_eq!(u16::from_le_bytes([c[2], c[3]]), 227, "wTotalLength");
		assert_eq!(c[4], 4, "four interfaces");
		let r = hid_report_descriptor();
		assert_eq!(r.len(), 337);
		assert_eq!(HID_REPORT_LEN, 337);
		assert_eq!(*r.last().unwrap(), 0xC0);
		assert_eq!(&hid_descriptor()[7..9], &[0x51, 0x01], "337 = 0x151");

		// Walk the endpoint descriptors: (address, attributes, wMaxPacketSize).
		let mut eps = Vec::new();
		let mut i = 0;
		while i < c.len() {
			let (len, ty) = (c[i] as usize, c[i + 1]);
			if ty == 0x05 {
				eps.push((c[i + 2], c[i + 3], u16::from_le_bytes([c[i + 4], c[i + 5]])));
			}
			i += len;
		}
		assert_eq!(i, c.len(), "descriptor chain is consistent");
		assert_eq!(
			eps,
			vec![
				(0x01, 0x09, 392),
				(0x82, 0x05, 196),
				(0x84, 0x03, 64),
				(0x03, 0x03, 64)
			]
		);

		let d = DEVICE_DESCRIPTOR;
		assert_eq!(u16::from_le_bytes([d[8], d[9]]), 0x054C);
		assert_eq!(u16::from_le_bytes([d[10], d[11]]), 0x0CE6);
		assert_eq!(d[7], 64);
	}

	#[test]
	fn default_features() {
		let (tx, _rx) = std::sync::mpsc::channel();
		let dev = Ds5Device::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55], 1.0, tx);
		let get = |id: u8| {
			let setup = [0xA1, 0x01, id, 0x03, ITF_HID as u8, 0, 64, 0];
			dev.control(&setup, &[]).unwrap()
		};
		let mac = get(0x09);
		assert_eq!(mac.len(), 20);
		assert_eq!(&mac[1..7], &[0x55, 0x44, 0x33, 0x22, 0x11, 0x02]);
		let fw = get(0x20);
		assert_eq!(fw.len(), 64);
		assert_eq!(&fw[1..4], b"Jul");
		let cal = get(0x05);
		assert_eq!(cal.len(), 41);
		assert_eq!(i16::from_le_bytes([cal[7], cal[8]]), 8640, "pitch plus @7");
		assert_eq!(
			i16::from_le_bytes([cal[33], cal[34]]),
			-8192,
			"acc z minus @33"
		);
		assert_eq!(get(0x22).len(), 64);
		assert_eq!(get(0xA0), vec![0xA0, 0]);

		dev.set_feature(0x09, &[9; 3]);
		assert_eq!(&get(0x09)[..5], &[0x09, 9, 9, 9, 0]);
		let mut edge = [0u8; 63];
		edge[21] = 0x44;
		dev.set_feature(0x20, &edge);
		assert_eq!(&get(0x20)[1..4], b"Jul", "Edge firmware report ignored");
	}

	#[test]
	fn mic_prebuffer_holds_until_filled_and_after_underrun() {
		let (tx, _rx) = std::sync::mpsc::channel();
		let dev = Ds5Device::new([2, 0, 0, 0, 0, 1], 1.0, tx);
		// SET_INTERFACE(mic, alt 1).
		dev.control(&[0x01, 0x0B, 0x01, 0x00, ITF_MIC as u8, 0, 0, 0], &[]).unwrap();
		dev.set_mic_prebuffer(2); // 2 ms = 2 packets
		let mut pkt = [0u8; MIC_PACKET_BYTES];
		let one_ms: Vec<i16> = vec![7; MIC_PACKET_BYTES / 2];
		dev.push_mic(&one_ms);
		dev.iso_in(EP_MIC_IN, &mut pkt);
		assert!(pkt.iter().all(|&b| b == 0), "1 ms queued < 2 ms prebuffer");
		dev.push_mic(&one_ms);
		dev.iso_in(EP_MIC_IN, &mut pkt);
		assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 7, "primed");
		dev.iso_in(EP_MIC_IN, &mut pkt);
		assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 7, "last ms still played");
		dev.push_mic(&one_ms);
		dev.iso_in(EP_MIC_IN, &mut pkt);
		assert!(pkt.iter().all(|&b| b == 0), "ran dry → prebuffer again");
	}
}
