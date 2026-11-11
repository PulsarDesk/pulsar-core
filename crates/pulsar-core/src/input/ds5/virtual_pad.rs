//! The host-side virtual wired DualSense: a USB/IP server exporting [`Ds5Device`],
//! attached to the local USB stack with the `usbip` CLI.
//!
//! Ported from ZerkSense5 (MIT, florinp93), `Core/BridgeService.cs` (`PlugIn` /
//! `Unplug`) and `Core/VirtualDualSense.cs`. Design: `docs/dualsense.md`.
//!
//! Order matches ZerkSense: the server is up before `usbip attach` runs (attach imports
//! from it), and on drop `usbip detach` runs before the server goes away.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::attach::{self, Attachment};
use super::device::{Ds5Device, EP_HID_IN};
use super::haptics;
use super::report::{neutral_usb_input, SetState, USB_INPUT_LEN};
use super::usbip::{trace, UsbIpServer};
use crate::input::{ds4_report_fields, GamepadKind, GamepadState, VirtualGamepad};

/// How to start a [`VirtualDs5`].
#[derive(Clone, Debug)]
pub struct VirtualDs5Config {
	/// TCP port on 127.0.0.1; 0 picks an ephemeral one (usbipd-win often holds 3240).
	pub port: u16,
	/// MAC served in feature 0x09 until a live pad supplies its own, display order
	/// (most significant octet first). Keep it stable per client and locally
	/// administered (see [`local_mac`]): `hid-playstation` rejects duplicate MACs.
	pub mac: [u8; 6],
	/// The usbip tool; `None` searches the default locations.
	pub usbip_exe: Option<PathBuf>,
	/// Haptics gain before quantising to s8 (ZerkSense allows 1.0..=2.0).
	pub haptics_gain: f32,
	/// Run `usbip attach` (needs admin / root). Off = server only, for tests or when an
	/// outside helper attaches.
	pub attach: bool,
}

impl Default for VirtualDs5Config {
	fn default() -> Self {
		Self {
			port: 0,
			mac: local_mac(b"pulsar"),
			usbip_exe: None,
			haptics_gain: 1.0,
			attach: true,
		}
	}
}

/// A stable, locally administered unicast MAC derived from `seed` (FNV-1a).
pub fn local_mac(seed: &[u8]) -> [u8; 6] {
	let mut h: u64 = 0xcbf2_9ce4_8422_2325;
	for &b in seed {
		h ^= b as u64;
		h = h.wrapping_mul(0x0100_0000_01b3);
	}
	let b = h.to_be_bytes();
	[(b[0] & 0xFC) | 0x02, b[1], b[2], b[3], b[4], b[5]]
}

/// What the game did with the virtual pad.
#[derive(Clone, Debug, PartialEq)]
pub enum Ds5HostEvent {
	/// Output report 0x02 (interrupt OUT or SET_REPORT): rumble, triggers, LEDs, audio.
	Output(SetState),
	/// One 3 kHz s8 stereo haptics frame (10.667 ms), from USB channels 3/4.
	Haptics(haptics::Frame),
	/// Speaker PCM from USB channels 1/2: 48 kHz interleaved stereo f32, one block per
	/// isochronous URB.
	Speaker(Vec<f32>),
	/// The speaker/haptics stream (interface 1) opened or closed.
	SpeakerOpen(bool),
	/// The mic stream (interface 2) opened or closed.
	MicOpen(bool),
	/// UAC feature-unit state, in whole dB (speaker −100..0, mic 0..48). Sent on every
	/// change and when the speaker stream opens (the pad needs it then).
	Volume {
		speaker_db: i16,
		mic_db: i16,
		speaker_mute: bool,
		mic_mute: bool,
	},
	/// SET_REPORT(Feature) the game sent: 0x80 test command / Edge unlock, 0x60-0x62
	/// Edge profile writes. Forward to a live pad (with the 0x53 CRC over BT) or drop.
	SetFeature { id: u8, data: Vec<u8> },
}

/// A virtual wired DualSense. Dropping it detaches the device and stops every thread.
pub struct VirtualDs5 {
	attachment: Option<Attachment>,
	server: UsbIpServer,
	device: Arc<Ds5Device>,
	/// For [`VirtualGamepad::apply`]: report counter and sensor clock.
	seq: AtomicU8,
	epoch: Instant,
}

impl VirtualDs5 {
	/// Start the server and, if `cfg.attach`, plug the device in with `usbip attach`.
	pub fn start(cfg: VirtualDs5Config) -> io::Result<(Self, Receiver<Ds5HostEvent>)> {
		let exe = if cfg.attach {
			Some(attach::locate(cfg.usbip_exe.as_deref())?)
		} else {
			None
		};
		let (tx, rx) = mpsc::channel();
		let device = Arc::new(Ds5Device::new(cfg.mac, cfg.haptics_gain, tx));
		let server = UsbIpServer::start(cfg.port, device.clone())?;
		let mut pad = Self {
			attachment: None,
			server,
			device,
			seq: AtomicU8::new(0),
			epoch: Instant::now(),
		};
		pad.set_input(&idle_input());
		if let Some(exe) = exe {
			pad.attachment = Some(attach::attach(&exe, pad.server.port())?);
			tracing::info!(
				"ds5: virtual wired DualSense plugged in (port {})",
				pad.port()
			);
		}
		Ok((pad, rx))
	}

	/// The game-facing input report (USB 0x01, 64 B). Completes a pending interrupt IN
	/// URB, or becomes the latest report.
	pub fn set_input(&self, usb_input: &[u8; USB_INPUT_LEN]) {
		self.device.set_input(usb_input);
		self.server.push_interrupt_in(EP_HID_IN, usb_input);
	}

	/// Replace a synthesized feature report with a live pad's (body without the id).
	pub fn set_feature(&self, id: u8, data: &[u8]) {
		self.device.set_feature(id, data);
	}

	/// Client mic audio, 48 kHz interleaved stereo s16. Ignored while the mic stream is
	/// closed.
	pub fn push_mic(&self, pcm48_stereo_s16: &[i16]) {
		self.device.push_mic(pcm48_stereo_s16);
	}

	/// The server's TCP port on 127.0.0.1.
	pub fn port(&self) -> u16 {
		self.server.port()
	}

	/// True while the OS (or a test client) has the device imported.
	pub fn attached(&self) -> bool {
		self.server.attached()
	}
}

impl Drop for VirtualDs5 {
	/// ZerkSense's order: `usbip detach` while our server still answers (PLUGOUT
	/// without reattach; the vhci cancels its URBs and closes the socket itself), then
	/// close our side. Closing the socket FIRST is not harmless on usbip-win2: after a
	/// successful attach any network error makes it plug the device out and re-attach
	/// later (every 30 s to 8 min, ~2 h; maintainer in issue #175). So we also run
	/// `usbip attach --stop` for our server at the end, which cancels such attempts if
	/// the session dropped early.
	fn drop(&mut self) {
		let Some(mut a) = self.attachment.take() else {
			return;
		};
		trace!("ds5: unplug vhci port {:?}", a.vhci_port());
		let detached = a.detach();
		let deadline = Instant::now() + Duration::from_secs(3);
		while self.server.attached() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(20));
		}
		trace!(
			"ds5: usbip detach ok={detached}, session still open={}",
			self.server.attached()
		);
		self.server.disconnect();
		a.stop_reattach();
		trace!("ds5: unplug done");
	}
}

/// The report a wired pad sends at rest: neutral sticks, both touch points lifted,
/// battery full, USB data + power.
pub fn idle_input() -> [u8; USB_INPUT_LEN] {
	let mut r = neutral_usb_input();
	r[1 + 32] = 0x80; // touch point 0: not touching
	r[1 + 36] = 0x80; // touch point 1
	r[1 + 52] = 0x2A; // power state 2 (full), level 10
	r[1 + 53] = 0x18; // USB data + USB power
	r
}

/// A USB input report from a normalized pad state (for clients without raw DS5 input).
pub fn usb_input_from_state(state: &GamepadState, seq: u8, sensor_ts: u32) -> [u8; USB_INPUT_LEN] {
	let (buttons, special, lx, ly, rx, ry) = ds4_report_fields(state);
	let mut r = idle_input();
	let s = &mut r[1..];
	s[0] = lx;
	s[1] = ly;
	s[2] = rx;
	s[3] = ry;
	s[4] = state.left_trigger;
	s[5] = state.right_trigger;
	s[6] = seq;
	// Hat + face buttons and L1..R3 share the DS4 bit layout.
	s[7] = buttons as u8;
	s[8] = (buttons >> 8) as u8;
	s[9] = special & 0x01;
	s[27..31].copy_from_slice(&sensor_ts.to_le_bytes());
	r
}

impl VirtualGamepad for VirtualDs5 {
	fn kind(&self) -> GamepadKind {
		GamepadKind::Ds5
	}

	fn apply(&mut self, state: &GamepadState) {
		let seq = self.seq.fetch_add(1, Ordering::Relaxed);
		// Sensor timestamp ticks in 1/3 µs.
		let ts = (self.epoch.elapsed().as_micros() as u64 * 3) as u32;
		self.set_input(&usb_input_from_state(state, seq, ts));
	}
}

#[cfg(test)]
mod tests {
	use super::super::device::{ITF_HID, ITF_SPEAKER};
	use super::super::usbip::*;
	use super::*;
	use crate::input::button;
	use std::io::{Read, Write};
	use std::net::TcpStream;
	use std::time::Duration;

	fn start() -> (VirtualDs5, Receiver<Ds5HostEvent>) {
		VirtualDs5::start(VirtualDs5Config {
			attach: false,
			..Default::default()
		})
		.unwrap()
	}

	fn be32(b: &[u8], o: usize) -> u32 {
		u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
	}

	/// A minimal in-process USB/IP client.
	struct Client {
		s: TcpStream,
		seq: u32,
	}

	struct Ret {
		cmd: u32,
		seq: u32,
		status: i32,
		actual: u32,
		packets: u32,
		data: Vec<u8>,
		iso: Vec<[u32; 4]>,
	}

	impl Client {
		fn import(port: u16) -> Self {
			let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
			s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
			let mut req = op_header(0x8003, 0).to_vec();
			let mut bus = [0u8; 32];
			bus[..3].copy_from_slice(b"1-1");
			req.extend_from_slice(&bus);
			s.write_all(&req).unwrap();
			let mut rep = [0u8; 8 + DEVICE_RECORD_LEN];
			s.read_exact(&mut rep).unwrap();
			assert_eq!(&rep[..8], &op_header(0x0003, 0));
			assert_eq!(&rep[8 + 256..8 + 259], b"1-1");
			Self { s, seq: 0 }
		}

		fn submit(
			&mut self,
			dir: u32,
			ep: u32,
			len: u32,
			setup: [u8; 8],
			out: &[u8],
			iso: &[(u32, u32)],
		) -> u32 {
			self.seq += 1;
			let mut h = [0u8; URB_HEADER_LEN];
			let mut put = |o: usize, v: u32| h[o..o + 4].copy_from_slice(&v.to_be_bytes());
			put(0, CMD_SUBMIT);
			put(4, self.seq);
			put(8, 0x0001_0001);
			put(12, dir);
			put(16, ep);
			put(24, len);
			put(
				32,
				if iso.is_empty() {
					u32::MAX
				} else {
					iso.len() as u32
				},
			);
			h[40..48].copy_from_slice(&setup);
			let mut b = h.to_vec();
			b.extend_from_slice(out);
			for &(off, l) in iso {
				for v in [off, l, 0, 0] {
					b.extend_from_slice(&v.to_be_bytes());
				}
			}
			self.s.write_all(&b).unwrap();
			self.seq
		}

		fn unlink(&mut self, victim: u32) -> u32 {
			self.seq += 1;
			let mut h = [0u8; URB_HEADER_LEN];
			h[0..4].copy_from_slice(&CMD_UNLINK.to_be_bytes());
			h[4..8].copy_from_slice(&self.seq.to_be_bytes());
			h[20..24].copy_from_slice(&victim.to_be_bytes());
			self.s.write_all(&h).unwrap();
			self.seq
		}

		/// Read one RET_*. `dir_in` / `iso` tell how much payload follows (the reply
		/// itself doesn't carry direction).
		fn ret(&mut self, dir_in: bool) -> Ret {
			let mut h = [0u8; URB_HEADER_LEN];
			self.s.read_exact(&mut h).unwrap();
			let cmd = be32(&h, 0);
			let actual = be32(&h, 24);
			let packets = be32(&h, 32);
			let mut data = vec![];
			if cmd == RET_SUBMIT && dir_in {
				data = vec![0u8; actual as usize];
				self.s.read_exact(&mut data).unwrap();
			}
			let mut iso = vec![];
			if cmd == RET_SUBMIT && packets != u32::MAX && packets > 0 {
				let mut d = vec![0u8; 16 * packets as usize];
				self.s.read_exact(&mut d).unwrap();
				iso = d
					.chunks_exact(16)
					.map(|c| [be32(c, 0), be32(c, 4), be32(c, 8), be32(c, 12)])
					.collect();
			}
			Ret {
				cmd,
				seq: be32(&h, 4),
				status: be32(&h, 20) as i32,
				actual,
				packets,
				data,
				iso,
			}
		}

		fn control_in(&mut self, setup: [u8; 8]) -> Ret {
			let len = u16::from_le_bytes([setup[6], setup[7]]) as u32;
			let seq = self.submit(DIR_IN, 0, len, setup, &[], &[]);
			let r = self.ret(true);
			assert_eq!((r.cmd, r.seq), (RET_SUBMIT, seq));
			r
		}

		fn control_out(&mut self, setup: [u8; 8], data: &[u8]) -> Ret {
			let seq = self.submit(DIR_OUT, 0, data.len() as u32, setup, data, &[]);
			let r = self.ret(false);
			assert_eq!((r.cmd, r.seq), (RET_SUBMIT, seq));
			r
		}
	}

	fn next_event(
		rx: &Receiver<Ds5HostEvent>,
		pred: impl Fn(&Ds5HostEvent) -> bool,
	) -> Ds5HostEvent {
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			let left = deadline.saturating_duration_since(Instant::now());
			let e = rx.recv_timeout(left).expect("event");
			if pred(&e) {
				return e;
			}
		}
	}

	#[test]
	fn devlist() {
		let (pad, _rx) = start();
		let mut s = TcpStream::connect(("127.0.0.1", pad.port())).unwrap();
		s.write_all(&op_header(0x8005, 0)).unwrap();
		let mut b = Vec::new();
		s.read_to_end(&mut b).unwrap();
		assert_eq!(b.len(), 8 + 4 + DEVICE_RECORD_LEN + 4 * 4);
		assert_eq!(&b[..8], &op_header(0x0005, 0));
		assert_eq!(be32(&b, 8), 1);
		let rec = &b[12..12 + DEVICE_RECORD_LEN];
		assert_eq!(&rec[300..304], &[0x05, 0x4C, 0x0C, 0xE6]);
		assert_eq!(rec[311], 4);
		assert_eq!(
			&b[12 + DEVICE_RECORD_LEN + 12..],
			&[3, 0, 0, 0],
			"HID interface last"
		);
	}

	#[test]
	fn usbip_session_end_to_end() {
		let (pad, rx) = start();
		let mut c = Client::import(pad.port());
		assert!(pad.attached());

		// A second import is refused while a session exists.
		{
			let mut s = TcpStream::connect(("127.0.0.1", pad.port())).unwrap();
			let mut req = op_header(0x8003, 0).to_vec();
			let mut bus = [0u8; 32];
			bus[..3].copy_from_slice(b"1-1");
			req.extend_from_slice(&bus);
			s.write_all(&req).unwrap();
			let mut rep = [0u8; 8];
			s.read_exact(&mut rep).unwrap();
			assert_eq!(rep, op_header(0x0003, 1));
		}

		// GET_DESCRIPTOR device / configuration.
		let r = c.control_in([0x80, 0x06, 0x00, 0x01, 0, 0, 18, 0]);
		assert_eq!(r.status, 0);
		assert_eq!(r.data.len(), 18);
		assert_eq!(&r.data[8..12], &[0x4C, 0x05, 0xE6, 0x0C]);
		let r = c.control_in([0x80, 0x06, 0x00, 0x02, 0, 0, 9, 0]);
		assert_eq!(r.data.len(), 9, "truncated to wLength");
		assert_eq!(u16::from_le_bytes([r.data[2], r.data[3]]), 227);
		let r = c.control_in([0x80, 0x06, 0x00, 0x02, 0, 0, 0xFF, 0]);
		assert_eq!(r.data.len(), 227);
		let r = c.control_in([0x81, 0x06, 0x00, 0x22, ITF_HID as u8, 0, 0x51, 0x01]);
		assert_eq!(r.data.len(), 337);
		// Unknown descriptor type → STALL.
		let r = c.control_in([0x80, 0x06, 0x00, 0x07, 0, 0, 8, 0]);
		assert_eq!(r.status, -EPIPE);

		// Open the speaker stream.
		let r = c.control_out([0x01, 0x0B, 0x01, 0x00, ITF_SPEAKER as u8, 0, 0, 0], &[]);
		assert_eq!(r.status, 0);
		assert_eq!(
			next_event(&rx, |e| matches!(e, Ds5HostEvent::SpeakerOpen(_))),
			Ds5HostEvent::SpeakerOpen(true)
		);

		// Iso OUT: 32 packets × 48 frames = 1536 frames = 3 haptics frames.
		let packets = 32u32;
		let mut pcm = Vec::new();
		for _ in 0..packets * 48 {
			for v in [16384i16, -16384, 8192, -8192] {
				pcm.extend_from_slice(&v.to_le_bytes());
			}
		}
		let iso: Vec<(u32, u32)> = (0..packets).map(|i| (i * 384, 384)).collect();
		let t0 = Instant::now();
		let seq = c.submit(DIR_OUT, 1, pcm.len() as u32, [0; 8], &pcm, &iso);
		let r = c.ret(false);
		assert!(
			t0.elapsed() >= Duration::from_millis(30),
			"paced at 1 ms per packet"
		);
		assert_eq!((r.cmd, r.seq, r.status), (RET_SUBMIT, seq, 0));
		assert_eq!(r.actual, pcm.len() as u32);
		assert_eq!(r.packets, packets);
		assert!(r.iso.iter().all(|d| d[2] == 384));
		let Ds5HostEvent::Speaker(spk) = next_event(&rx, |e| matches!(e, Ds5HostEvent::Speaker(_)))
		else {
			unreachable!()
		};
		assert_eq!(spk.len(), (packets * 48 * 2) as usize);
		assert_eq!((spk[0], spk[1]), (0.5, -0.5));
		let mut haptics = 0;
		while let Ok(e) = rx.recv_timeout(Duration::from_millis(200)) {
			if let Ds5HostEvent::Haptics(f) = e {
				haptics += 1;
				assert_eq!((f[0] as i8, f[1] as i8), (32, -32), "0.25 × 127");
			}
		}
		assert_eq!(haptics, 3);

		// Interrupt OUT 0x02 → Output.
		let mut out = vec![0x02u8, 0x0C, 0x14];
		out.resize(48, 0);
		out[11] = 0x21;
		let seq = c.submit(DIR_OUT, 3, out.len() as u32, [0; 8], &out, &[]);
		let r = c.ret(false);
		assert_eq!((r.seq, r.status, r.actual), (seq, 0, 48));
		let Ds5HostEvent::Output(s) = next_event(&rx, |e| matches!(e, Ds5HostEvent::Output(_)))
		else {
			unreachable!()
		};
		assert_eq!(&s.0[..2], &[0x0C, 0x14]);
		assert_eq!(s.0[10], 0x21);
		assert!(s.has_triggers());

		// SET_REPORT(Output 0x02) over EP0 → Output too.
		let r = c.control_out(
			[0x21, 0x09, 0x02, 0x02, ITF_HID as u8, 0, 3, 0],
			&[0x02, 0x01, 0x00],
		);
		assert_eq!(r.status, 0);
		let Ds5HostEvent::Output(s) = next_event(&rx, |e| matches!(e, Ds5HostEvent::Output(_)))
		else {
			unreachable!()
		};
		assert_eq!(s.0[0], 0x01);

		// Interrupt IN: the latest set_input completes the next URB (coalesced).
		let mut a = idle_input();
		a[1] = 0x11;
		let mut b = idle_input();
		b[1] = 0x22;
		pad.set_input(&a);
		pad.set_input(&b);
		let seq = c.submit(DIR_IN, 4, 64, [0; 8], &[], &[]);
		let r = c.ret(true);
		assert_eq!((r.seq, r.status), (seq, 0));
		assert_eq!(r.data, b.to_vec(), "only the newest report kept");
		// Pending URB, then input.
		let seq = c.submit(DIR_IN, 4, 64, [0; 8], &[], &[]);
		std::thread::sleep(Duration::from_millis(50));
		pad.set_input(&a);
		let r = c.ret(true);
		assert_eq!((r.seq, r.data), (seq, a.to_vec()));

		// GET_REPORT(Input) answers the last input.
		let r = c.control_in([0xA1, 0x01, 0x01, 0x01, ITF_HID as u8, 0, 64, 0]);
		assert_eq!(r.data, a.to_vec());

		// UNLINK of a pending interrupt IN → -ECONNRESET, and no RET_SUBMIT for it.
		let victim = c.submit(DIR_IN, 4, 64, [0; 8], &[], &[]);
		std::thread::sleep(Duration::from_millis(50));
		let useq = c.unlink(victim);
		let r = c.ret(false);
		assert_eq!((r.cmd, r.seq, r.status), (RET_UNLINK, useq, -ECONNRESET));
		// The unlinked URB must not swallow this input: the next URB gets it.
		pad.set_input(&b);
		let seq = c.submit(DIR_IN, 4, 64, [0; 8], &[], &[]);
		let r = c.ret(true);
		assert_eq!((r.cmd, r.seq, r.data), (RET_SUBMIT, seq, b.to_vec()));
		// UNLINK of an already completed URB → 0.
		let useq = c.unlink(seq);
		let r = c.ret(false);
		assert_eq!((r.cmd, r.seq, r.status), (RET_UNLINK, useq, 0));

		// Mic: open, push, iso IN returns it; underrun → zeros.
		c.control_out([0x01, 0x0B, 0x01, 0x00, 2, 0, 0, 0], &[]);
		next_event(&rx, |e| *e == Ds5HostEvent::MicOpen(true));
		let mic: Vec<i16> = (0..96).map(|i| i as i16 + 1).collect();
		pad.push_mic(&mic);
		let seq = c.submit(DIR_IN, 2, 2 * 196, [0; 8], &[], &[(0, 196), (196, 196)]);
		let r = c.ret(true);
		assert_eq!((r.seq, r.status, r.packets), (seq, 0, 2));
		assert_eq!(r.data.len(), 384, "192 B per packet, packed");
		assert_eq!(r.iso[1][..3], [196, 196, 192], "offset kept, actual 192");
		assert_eq!(i16::from_le_bytes([r.data[0], r.data[1]]), 1);
		assert_eq!(i16::from_le_bytes([r.data[190], r.data[191]]), 96);
		assert!(r.data[192..].iter().all(|&x| x == 0), "underrun → zeros");

		// UAC volume SET_CUR → Volume.
		c.control_out(
			[0x21, 0x01, 0x00, 0x02, 0x00, 0x02, 2, 0],
			&(-20i16 * 256).to_le_bytes(),
		);
		assert_eq!(
			next_event(&rx, |e| matches!(e, Ds5HostEvent::Volume { .. })),
			Ds5HostEvent::Volume {
				speaker_db: -20,
				mic_db: 48,
				speaker_mute: false,
				mic_mute: false
			}
		);

		// Disconnect closes streams.
		drop(c);
		next_event(&rx, |e| *e == Ds5HostEvent::SpeakerOpen(false));
		let deadline = Instant::now() + Duration::from_secs(2);
		while pad.attached() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(10));
		}
		assert!(!pad.attached());
	}

	/// What a UAC1 class driver (usbaudio.sys, snd-usb-audio) does to start rendering,
	/// inferred from the UAC1 spec and the device's descriptors: probe the feature units,
	/// select alt 1, set and read back the endpoint sample rate, keep several iso OUT
	/// URBs in flight (ISO_ASAP), then select alt 0. Everything must be answered.
	#[test]
	fn uac1_render_start_sequence() {
		let (pad, rx) = start();
		let mut c = Client::import(pad.port());

		for entity in [0x02u8, 0x05] {
			for ch in [0u8, 1] {
				for req in [0x81u8, 0x82, 0x83, 0x84] {
					let r = c.control_in([0xA1, req, ch, 0x02, 0x00, entity, 2, 0]);
					assert_eq!(
						(r.status, r.data.len()),
						(0, 2),
						"volume {req:#x} fu {entity}"
					);
				}
			}
			let r = c.control_in([0xA1, 0x81, 0x00, 0x01, 0x00, entity, 1, 0]);
			assert_eq!((r.status, r.data.len()), (0, 1), "mute GET_CUR");
		}
		let r = c.control_in([0xA1, 0x83, 0x00, 0x02, 0x00, 0x02, 2, 0]);
		assert_eq!(r.data, 0i16.to_le_bytes(), "speaker max 0 dB");
		let r = c.control_in([0xA1, 0x82, 0x00, 0x02, 0x00, 0x02, 2, 0]);
		assert_eq!(r.data, (-100i16 * 256).to_le_bytes(), "speaker min -100 dB");

		// SET_INTERFACE(1, 1), sample rate SET_CUR / GET_CUR on EP 0x01.
		assert_eq!(
			c.control_out([0x01, 0x0B, 0x01, 0x00, 0x01, 0x00, 0, 0], &[])
				.status,
			0
		);
		assert_eq!(
			next_event(&rx, |e| matches!(e, Ds5HostEvent::SpeakerOpen(_))),
			Ds5HostEvent::SpeakerOpen(true)
		);
		let r = c.control_out(
			[0x22, 0x01, 0x00, 0x01, 0x01, 0x00, 3, 0],
			&[0x80, 0xBB, 0x00],
		);
		assert_eq!((r.status, r.actual), (0, 3));
		let r = c.control_in([0xA2, 0x81, 0x00, 0x01, 0x01, 0x00, 3, 0]);
		assert_eq!((r.status, r.data), (0, vec![0x80, 0xBB, 0x00]));
		// GET_INTERFACE reads back alt 1.
		let r = c.control_in([0x81, 0x0A, 0, 0, 0x01, 0x00, 1, 0]);
		assert_eq!(r.data, vec![1]);

		// Three 8-packet URBs in flight, ISO_ASAP, silence: all complete, in order, paced.
		let pcm = vec![0u8; 8 * 384];
		let iso: Vec<(u32, u32)> = (0..8).map(|i| (i * 384, 384)).collect();
		let seqs: Vec<u32> = (0..3)
			.map(|_| c.submit(DIR_OUT, 1, pcm.len() as u32, [0; 8], &pcm, &iso))
			.collect();
		let t0 = Instant::now();
		for s in &seqs {
			let r = c.ret(false);
			assert_eq!((r.cmd, r.seq, r.status, r.packets), (RET_SUBMIT, *s, 0, 8));
			assert!(r.iso.iter().all(|d| d[2] == 384 && d[3] == 0));
		}
		assert!(
			t0.elapsed() >= Duration::from_millis(15),
			"24 ms of audio, paced"
		);

		assert_eq!(
			c.control_out([0x01, 0x0B, 0x00, 0x00, 0x01, 0x00, 0, 0], &[])
				.status,
			0
		);
		next_event(&rx, |e| *e == Ds5HostEvent::SpeakerOpen(false));
	}

	#[test]
	fn unlink_pending_iso_urb() {
		let (pad, _rx) = start();
		let mut c = Client::import(pad.port());
		// 200 packets = due in 200 ms; unlink it right away.
		let iso: Vec<(u32, u32)> = (0..200).map(|i| (i * 196, 196)).collect();
		let victim = c.submit(DIR_IN, 2, 200 * 196, [0; 8], &[], &iso);
		let useq = c.unlink(victim);
		let r = c.ret(false);
		assert_eq!((r.cmd, r.seq, r.status), (RET_UNLINK, useq, -ECONNRESET));
		// Nothing else arrives for the victim.
		c.s.set_read_timeout(Some(Duration::from_millis(400)))
			.unwrap();
		let mut h = [0u8; 1];
		assert!(c.s.read(&mut h).is_err(), "no RET_SUBMIT after unlink");
	}

	#[test]
	fn apply_synthesizes_usb_input() {
		let mut st = GamepadState::default();
		st.set(button::A, true);
		st.set(button::DPAD_RIGHT, true);
		st.set(button::RB, true);
		st.set(button::GUIDE, true);
		st.left_x = i16::MAX;
		st.left_y = i16::MAX; // up → 0x00 on the pad
		st.right_trigger = 200;
		let r = usb_input_from_state(&st, 7, 0x0102_0304);
		assert_eq!(r[0], 0x01);
		assert_eq!((r[1], r[2], r[3], r[4]), (0xFF, 0x00, 0x80, 0x80));
		assert_eq!((r[5], r[6], r[7]), (0, 200, 7));
		assert_eq!(r[8], 0x20 | 2, "cross + hat E");
		assert_eq!(r[9], 0x02 | 0x08, "R1 + R2 bit");
		assert_eq!(r[10], 0x01, "PS");
		assert_eq!(&r[28..32], &[4, 3, 2, 1]);
		assert_eq!(r[33] & 0x80, 0x80, "touch lifted");

		let (mut pad, _rx) = start();
		pad.apply(&st);
		assert_eq!(pad.kind(), GamepadKind::Ds5);
	}

	#[test]
	fn local_mac_is_stable_and_local() {
		let a = local_mac(b"client-1");
		assert_eq!(a, local_mac(b"client-1"));
		assert_ne!(a, local_mac(b"client-2"));
		assert_eq!(a[0] & 0x03, 0x02, "locally administered, unicast");
	}
}
