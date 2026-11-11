//! Client side: drive a physical DualSense from the host's DualSense stream.
//!
//! Platform-free logic only; the app supplies HID I/O through [`HidIo`] (SDL3's
//! `SDL_hid_*` on the desktop) and calls [`Pad::poll`] from its pad thread. On USB the
//! pad's haptics/speaker play through its own audio device (the app does that), so
//! [`Pad`] only writes state reports; on Bluetooth it also paces the audio reports
//! (0x32 / 0x39) on a steady 10.667 ms-per-frame clock.
//!
//! Pacing, prebuffering, the ±3 % drift trim, the 8-frame FIFO and "state before
//! audio" follow ZerkSense5 `AudioBridge` / `DualSenseBt` (MIT).

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::bt_audio::{AudioReports, OpusFrame, SpeakerOut, SPEAKER_OPUS_LEN};
use super::haptics::{Frame, FRAME_BYTES};
use super::report::{
	parse_bt_input, BtInput, BtSeq, SetState, SET_STATE_USED, USB_INPUT_ID, USB_INPUT_LEN,
};
use super::wire::Ds5Transport;
use super::Ds5Features;

/// Minimal HID access to one physical pad. `buf[0]` is always the report id.
pub trait HidIo {
	/// Write an output report.
	fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
	/// Read feature report `id` (`len` bytes incl. the id).
	fn get_feature(&mut self, id: u8, len: usize) -> io::Result<Vec<u8>>;
}

/// Feature reports read at connect, in this order (ZerkSense5 / DS5Dongle), with their
/// sizes. Reading 0x05 (calibration) is also what moves a BT pad to full 0x31 reports.
pub const INIT_FEATURES: [(u8, usize); 4] = [(0x09, 20), (0x20, 64), (0x22, 64), (0x05, 41)];

/// Milliseconds per BT haptics frame (32 samples at 3 kHz).
const FRAME: Duration = Duration::from_micros(10_667);
/// FIFO bound, in frames; the oldest is dropped when full.
const MAX_QUEUED: usize = 8;
/// Least gap between two state writes (coalescing; games may write 0x02 at 1 kHz).
const STATE_MIN_GAP: Duration = Duration::from_millis(4);

/// What a physical pad reported.
#[derive(Debug, PartialEq, Eq)]
pub enum PadInput {
	/// Input state in the USB 0x01 layout.
	State([u8; USB_INPUT_LEN]),
	/// One pad-mic Opus packet (BT only).
	Mic(Vec<u8>),
}

/// One host slot waiting to be played.
#[derive(Debug, Clone)]
struct Slot {
	haptics: Frame,
	speaker: Option<(SpeakerOut, OpusFrame)>,
}

/// Write counters, for the client's auto-degrade policy and the stats log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PadStats {
	pub state_writes: u64,
	pub audio_writes: u64,
	pub write_errors: u64,
	/// Slots dropped because the FIFO was full (the BT link can't keep up).
	pub overflows: u64,
	/// Times the FIFO ran dry while playing (host stream late or lost).
	pub underruns: u64,
	/// Longest single write, in microseconds.
	pub max_write_us: u64,
}

/// A physical DualSense being driven by the host.
pub struct Pad {
	transport: Ds5Transport,
	features: Ds5Features,
	seq: BtSeq,
	audio: AudioReports,
	/// Two frames per report (0x39 / double 0x32): half the BT reports. Default on.
	pub double: bool,
	pending_state: Option<SetState>,
	last_state: Option<Instant>,
	fifo: VecDeque<Slot>,
	playing: bool,
	next_due: Option<Instant>,
	mic_dirty: bool,
	stats: PadStats,
}

impl Pad {
	/// Open the pad: read its feature reports (returned so the client can forward them
	/// to the host as `Ds5Feature`), and on BT announce the mic state.
	pub fn open(
		io: &mut impl HidIo,
		transport: Ds5Transport,
		features: Ds5Features,
	) -> io::Result<(Self, Vec<(u8, Vec<u8>)>)> {
		let mut feats = Vec::new();
		for (id, len) in INIT_FEATURES {
			match io.get_feature(id, len) {
				Ok(f) => feats.push((id, f)),
				// 0x22 is undocumented; tolerate a pad that refuses it.
				Err(e) if id == 0x22 => tracing::debug!("ds5: feature 0x22 unavailable ({e})"),
				Err(e) => return Err(e),
			}
		}
		let mut pad = Self {
			transport,
			features,
			seq: BtSeq::default(),
			audio: AudioReports::default(),
			double: true,
			pending_state: None,
			last_state: None,
			fifo: VecDeque::new(),
			playing: false,
			next_due: None,
			mic_dirty: transport == Ds5Transport::Bt,
			stats: PadStats::default(),
		};
		pad.audio.mic = transport == Ds5Transport::Bt && features.mic;
		Ok((pad, feats))
	}

	pub fn stats(&self) -> PadStats {
		self.stats
	}

	/// Change which features reach the pad (a setting changed, or auto-degrade).
	pub fn set_features(&mut self, f: Ds5Features) {
		let mic = self.transport == Ds5Transport::Bt && f.mic;
		if mic != self.audio.mic {
			self.audio.mic = mic;
			self.mic_dirty = true;
		}
		if !f.haptics && !f.speaker {
			self.fifo.clear();
			self.playing = false;
		}
		self.features = f;
	}

	/// Decode one input report read from the pad.
	pub fn on_input(&self, buf: &[u8]) -> Option<PadInput> {
		match self.transport {
			Ds5Transport::Bt => match parse_bt_input(buf, true)? {
				BtInput::State(s) => Some(PadInput::State(s)),
				BtInput::Mic(opus) => Some(PadInput::Mic(opus.to_vec())),
			},
			Ds5Transport::Usb => {
				let r: [u8; USB_INPUT_LEN] = buf.get(..USB_INPUT_LEN)?.try_into().ok()?;
				(r[0] == USB_INPUT_ID).then_some(PadInput::State(r))
			}
		}
	}

	/// The host's latest output state; written on the next [`Pad::poll`] (coalesced).
	pub fn set_output(&mut self, mut s: SetState) {
		s.mask(&self.features);
		self.pending_state = Some(s);
	}

	/// Queue one host slot (BT only; a USB pad plays audio through its audio device).
	pub fn push_slot(&mut self, haptics: Option<Frame>, speaker: Option<(SpeakerOut, Vec<u8>)>) {
		if self.transport != Ds5Transport::Bt {
			return;
		}
		let haptics = if self.features.haptics { haptics } else { None };
		let speaker = if self.features.speaker {
			speaker.and_then(|(o, v)| {
				let mut f = [0u8; SPEAKER_OPUS_LEN];
				let n = v.len().min(SPEAKER_OPUS_LEN);
				f[..n].copy_from_slice(&v[..n]);
				(n > 0).then_some((o, f))
			})
		} else {
			None
		};
		if haptics.is_none() && speaker.is_none() {
			return;
		}
		if self.fifo.len() >= MAX_QUEUED {
			self.fifo.pop_front();
			self.stats.overflows += 1;
		}
		self.fifo.push_back(Slot {
			haptics: haptics.unwrap_or([0; FRAME_BYTES]),
			speaker,
		});
	}

	fn frames_per_report(&self) -> usize {
		if self.double {
			2
		} else {
			1
		}
	}

	/// Do whatever is due at `now`. Returns how long the caller may sleep before the
	/// next call (it should also wake on new input from the host).
	pub fn poll(&mut self, now: Instant, io: &mut impl HidIo) -> Duration {
		// State first: it never waits behind audio.
		if let Some(s) = self.pending_state {
			let ready = self
				.last_state
				.is_none_or(|t| now.duration_since(t) >= STATE_MIN_GAP);
			if ready {
				self.pending_state = None;
				self.last_state = Some(now);
				match self.transport {
					Ds5Transport::Usb => {
						let r = s.usb_report();
						self.write(io, &r[..1 + SET_STATE_USED], false);
					}
					Ds5Transport::Bt => {
						let r = s.bt_report(&mut self.seq);
						self.write(io, &r, false);
					}
				}
			}
		}
		if self.mic_dirty && self.transport == Ds5Transport::Bt {
			self.mic_dirty = false;
			let r = self.audio.mic_status(&mut self.seq);
			self.write(io, &r, true);
		}
		let state_wait = match (self.pending_state, self.last_state) {
			(Some(_), Some(t)) => STATE_MIN_GAP.saturating_sub(now.duration_since(t)),
			_ => Duration::from_millis(50),
		};
		state_wait.min(self.poll_audio(now, io))
	}

	fn poll_audio(&mut self, now: Instant, io: &mut impl HidIo) -> Duration {
		let n = self.frames_per_report();
		if !self.playing {
			// Prebuffer two reports' worth before starting the clock.
			if self.fifo.len() < 2 * n {
				return Duration::from_millis(50);
			}
			self.playing = true;
			self.next_due = Some(now);
		}
		let due = self.next_due.unwrap_or(now);
		if now < due {
			return due - now;
		}
		if self.fifo.len() < n {
			self.playing = false;
			self.next_due = None;
			self.stats.underruns += 1;
			return Duration::from_millis(50);
		}
		let slots: Vec<Slot> = self.fifo.drain(..n).collect();
		self.send_audio(&slots, io);
		// Steady clock with a ±3 % trim so the host's USB clock and our send clock
		// can't drift the FIFO empty or full; resync after a stall.
		let left = self.fifo.len();
		let target = 2 * n;
		let trim = if left > target {
			0.97
		} else if left < n {
			1.03
		} else {
			1.0
		};
		let period = FRAME.mul_f64(n as f64 * trim);
		let mut next = due + period;
		if now.saturating_duration_since(next) > period * 2 {
			next = now + period;
		}
		self.next_due = Some(next);
		next.saturating_duration_since(now)
	}

	fn send_audio(&mut self, slots: &[Slot], io: &mut impl HidIo) {
		let haptics: Vec<Frame> = slots.iter().map(|s| s.haptics).collect();
		let all_speaker = slots.iter().all(|s| s.speaker.is_some());
		let out = slots
			.iter()
			.find_map(|s| s.speaker.map(|(o, _)| o))
			.unwrap_or(SpeakerOut::Speaker);
		match (slots.len(), all_speaker) {
			(2, true) => {
				let opus = [slots[0].speaker.unwrap().1, slots[1].speaker.unwrap().1];
				let r = self
					.audio
					.double(&[haptics[0], haptics[1]], &opus, out, &mut self.seq);
				self.write(io, &r, true);
			}
			(1, true) => {
				let opus = slots[0].speaker.unwrap().1;
				let r = self.audio.single(&haptics[0], &opus, out, &mut self.seq);
				self.write(io, &r, true);
			}
			// A slot without speaker audio: haptics-only (an empty Opus frame would click).
			_ => {
				let r = self.audio.haptics_only(&haptics, &mut self.seq);
				self.write(io, &r, true);
			}
		}
	}

	fn write(&mut self, io: &mut impl HidIo, buf: &[u8], audio: bool) {
		let t = Instant::now();
		match io.write(buf) {
			Ok(_) => {
				if audio {
					self.stats.audio_writes += 1;
				} else {
					self.stats.state_writes += 1;
				}
			}
			Err(e) => {
				self.stats.write_errors += 1;
				tracing::debug!("ds5: write of report 0x{:02x} failed: {e}", buf[0]);
			}
		}
		let us = t.elapsed().as_micros() as u64;
		self.stats.max_write_us = self.stats.max_write_us.max(us);
	}
}

#[cfg(test)]
mod tests {
	use super::super::{crc, report};
	use super::*;

	#[derive(Default)]
	struct Mock {
		writes: Vec<Vec<u8>>,
		refuse_0x22: bool,
	}

	impl HidIo for Mock {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.writes.push(buf.to_vec());
			Ok(buf.len())
		}
		fn get_feature(&mut self, id: u8, len: usize) -> io::Result<Vec<u8>> {
			if id == 0x22 && self.refuse_0x22 {
				return Err(io::Error::other("no"));
			}
			let mut v = vec![0u8; len];
			v[0] = id;
			Ok(v)
		}
	}

	fn bt_pad(io: &mut Mock) -> Pad {
		let (pad, feats) = Pad::open(io, Ds5Transport::Bt, Ds5Features::all()).unwrap();
		assert_eq!(
			feats.iter().map(|f| f.0).collect::<Vec<_>>(),
			vec![0x09, 0x20, 0x22, 0x05]
		);
		pad
	}

	#[test]
	fn open_tolerates_missing_0x22_and_announces_mic() {
		let mut io = Mock {
			refuse_0x22: true,
			..Default::default()
		};
		let (mut pad, feats) = Pad::open(&mut io, Ds5Transport::Bt, Ds5Features::all()).unwrap();
		assert_eq!(feats.len(), 3);
		pad.poll(Instant::now(), &mut io);
		assert_eq!(io.writes.len(), 1);
		assert_eq!(&io.writes[0][..5], &[0x32, 0x00, 0x91, 1, 0x03], "mic on");
		assert!(crc::verify(crc::SEED_OUTPUT, &io.writes[0]));
	}

	#[test]
	fn state_is_masked_and_coalesced() {
		let mut io = Mock::default();
		let (mut pad, _) = Pad::open(
			&mut io,
			Ds5Transport::Usb,
			Ds5Features {
				triggers: false,
				..Ds5Features::all()
			},
		)
		.unwrap();
		let t0 = Instant::now();
		let mut s = SetState::default();
		s.0[0] = report::flag::B0_RIGHT_TRIGGER | report::flag::B0_RUMBLE_EMULATION;
		s.0[3] = 99;
		pad.set_output(s);
		pad.poll(t0, &mut io);
		assert_eq!(io.writes.len(), 1);
		let w = &io.writes[0];
		assert_eq!(w.len(), 48, "USB 0x02 = id + 47");
		assert_eq!(w[0], 0x02);
		assert_eq!(
			w[1] & report::flag::B0_RIGHT_TRIGGER,
			0,
			"triggers masked off"
		);
		assert_eq!(w[4], 99, "rumble kept");
		// Two more updates within 4 ms collapse into one write of the latest.
		s.0[3] = 10;
		pad.set_output(s);
		s.0[3] = 20;
		pad.set_output(s);
		let wait = pad.poll(t0 + Duration::from_millis(1), &mut io);
		assert_eq!(io.writes.len(), 1);
		assert!(wait <= Duration::from_millis(3));
		pad.poll(t0 + Duration::from_millis(5), &mut io);
		assert_eq!(io.writes.len(), 2);
		assert_eq!(io.writes[1][4], 20);
	}

	#[test]
	fn bt_audio_prebuffers_paces_and_prefers_state() {
		let mut io = Mock::default();
		let mut pad = bt_pad(&mut io);
		let t0 = Instant::now();
		pad.poll(t0, &mut io);
		io.writes.clear(); // mic status
		let opus = vec![0x11u8; SPEAKER_OPUS_LEN];
		for _ in 0..3 {
			pad.push_slot(
				Some([1; FRAME_BYTES]),
				Some((SpeakerOut::Speaker, opus.clone())),
			);
		}
		pad.poll(t0, &mut io);
		assert!(
			io.writes.is_empty(),
			"needs 4 slots (2 double reports) before playing"
		);
		pad.push_slot(
			Some([2; FRAME_BYTES]),
			Some((SpeakerOut::Speaker, opus.clone())),
		);
		pad.set_output(SetState::default());
		let wait = pad.poll(t0, &mut io);
		assert_eq!(io.writes.len(), 2);
		assert_eq!(io.writes[0][0], 0x31, "state goes first");
		assert_eq!(io.writes[1][0], 0x39, "two slots with speaker = 0x39");
		assert_eq!(
			io.writes[1][1] >> 4,
			(io.writes[0][1] >> 4) + 1,
			"one shared seq"
		);
		assert!(wait > Duration::from_millis(20) && wait <= Duration::from_millis(22));
		// Not due yet.
		pad.poll(t0 + Duration::from_millis(10), &mut io);
		assert_eq!(io.writes.len(), 2);
		// Due: the second pair goes out.
		pad.poll(t0 + Duration::from_millis(22), &mut io);
		assert_eq!(io.writes.len(), 3);
		// FIFO empty at the next due time → underrun, playback stops.
		pad.poll(t0 + Duration::from_millis(45), &mut io);
		assert_eq!(pad.stats().underruns, 1);
		assert_eq!(pad.stats().audio_writes, 3, "mic status + two 0x39");
	}

	#[test]
	fn haptics_only_when_a_slot_has_no_speaker_and_overflow_drops_oldest() {
		let mut io = Mock::default();
		let mut pad = bt_pad(&mut io);
		pad.set_features(Ds5Features {
			speaker: false,
			..Ds5Features::all()
		});
		for i in 0..10u8 {
			pad.push_slot(
				Some([i; FRAME_BYTES]),
				Some((SpeakerOut::Speaker, vec![1; 10])),
			);
		}
		assert_eq!(pad.stats().overflows, 2);
		let t0 = Instant::now();
		pad.poll(t0, &mut io);
		let a = io
			.writes
			.iter()
			.find(|w| w[0] == 0x32 && w[2] == 0x91 && w[3] == 4)
			.unwrap();
		assert_eq!(a[8], 0xD2, "double haptics-only");
		assert_eq!(a[10], 2, "slots 0 and 1 were dropped");
	}

	#[test]
	fn usb_ignores_audio_slots_and_parses_input() {
		let mut io = Mock::default();
		let (mut pad, _) = Pad::open(&mut io, Ds5Transport::Usb, Ds5Features::all()).unwrap();
		pad.push_slot(Some([1; FRAME_BYTES]), None);
		pad.poll(Instant::now(), &mut io);
		assert!(io.writes.is_empty());
		let r = report::neutral_usb_input();
		assert_eq!(pad.on_input(&r), Some(PadInput::State(r)));
		assert_eq!(pad.on_input(&[0x31; 78]), None);
	}
}
