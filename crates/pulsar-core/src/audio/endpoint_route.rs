//! DualSense-aware host audio: which capture feeds the stream while a virtual DualSense is
//! plugged in, and the PCM work the default-endpoint loopback needs (pure, all platforms).
//!
//! The Windows host normally streams the system mix with process loopback (every process but
//! Pulsar's tree). That capture is endpoint-agnostic: what a game renders to the virtual
//! pad's own endpoint ("Speakers (N- DualSense Wireless Controller)", 4 ch: 1/2 speaker,
//! 3/4 haptics) is captured too and played in the client's normal audio. So while a pad is
//! plugged the host taps the **default render endpoint** instead (WASAPI loopback), which
//! never carries the pad endpoint's audio.
//!
//! That loopback sits after the endpoint volume (and mute): the host's volume would reach the
//! client. [`compensation_gain`] undoes the endpoint's master attenuation; when there is
//! nothing to recover (muted, volume 0) the [`Router`] falls back to process loopback. It
//! also falls back when the default endpoint IS a DualSense, or the endpoint capture fails.
//! The Windows side lives in `endpoint_watch.rs` (volume/mute/default watch) and the
//! desktop host (the capture switch).

use std::time::{Duration, Instant};

/// The largest attenuation [`compensation_gain`] undoes (dB). Windows' software volume
/// bottoms out around −65 dB on common endpoints; past this there is too little signal left.
pub const MAX_COMPENSATION_DB: f32 = 70.0;

/// What the watcher sees on the default render endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointObservation {
	/// MMDevice endpoint id.
	pub id: String,
	/// The endpoint belongs to a DualSense (its name says so) — a virtual pad's own endpoint
	/// made default. Its loopback carries the pad audio, so it is never used.
	pub is_dualsense: bool,
	pub muted: bool,
	/// `IAudioEndpointVolume::GetMasterVolumeLevel` (dB, ≤ 0). This, not the slider scalar,
	/// is the attenuation the audio engine applies: the scalar is an audio-taper position
	/// (20 % is about −25 dB, not −14 dB).
	pub level_db: f32,
	/// `GetMasterVolumeLevelScalar` (0..1): only used to tell "volume 0".
	pub scalar: f32,
	/// The endpoint applies volume in hardware (`ENDPOINT_HARDWARE_SUPPORT_VOLUME`): the
	/// loopback is then taken before it, and nothing needs undoing.
	pub hw_volume: bool,
}

/// The gain that restores full level on `obs`'s loopback, or `None` when nothing can be
/// recovered (muted, volume 0, past [`MAX_COMPENSATION_DB`]) — the process loopback is used
/// then.
pub fn compensation_gain(obs: &EndpointObservation) -> Option<f32> {
	if obs.muted || !(obs.scalar > 0.0) || !obs.level_db.is_finite() {
		return None;
	}
	if obs.hw_volume {
		return Some(1.0);
	}
	let att = (-obs.level_db).max(0.0);
	if att > MAX_COMPENSATION_DB {
		return None;
	}
	Some(10f32.powf(att / 20.0))
}

/// Whether any of an endpoint's names (friendly, adapter, description) is a DualSense's.
pub fn is_dualsense_name<S: AsRef<str>>(names: &[Option<S>]) -> bool {
	names.iter().flatten().any(|n| {
		let n = n.as_ref().to_ascii_lowercase();
		n.contains("dualsense")
	})
}

/// Which capture feeds the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
	/// Process loopback (today's path; also every fallback).
	Process,
	/// Loopback of the default render endpoint, volume-compensated.
	Endpoint,
}

/// The endpoint capture as the caller last saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture<'a> {
	/// Not running.
	Idle,
	/// Started on this endpoint id, no PCM yet.
	Starting(&'a str),
	/// Delivering PCM from this endpoint id.
	Live(&'a str),
	/// It ended on its own (open failed, device gone, unusable format). Report once; the
	/// caller then treats it as [`Capture::Idle`].
	Failed,
}

/// What to do with the endpoint capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
	None,
	/// Start it on this endpoint id.
	Start(String),
	/// Stop it (the stream is on process loopback meanwhile).
	Stop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
	pub source: Source,
	pub action: Action,
	/// The compensation gain for the endpoint capture (`None`: nothing to recover).
	pub gain: Option<f32>,
}

/// One step's inputs.
#[derive(Clone, Copy, Debug)]
pub struct RouteInput<'a> {
	/// A virtual DualSense with an audio endpoint is plugged in on this host.
	pub pad_plugged: bool,
	/// This stream may use the endpoint loopback (see the desktop host: not with a
	/// host-silent redirect armed, not for same-host co-op).
	pub allowed: bool,
	/// The default render endpoint (`None`: none, or it could not be read).
	pub endpoint: Option<&'a EndpointObservation>,
	pub capture: Capture<'a>,
}

/// First retry delay after the endpoint capture failed; doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The source decision. Switches to the endpoint loopback only once it delivers PCM, so a
/// switch never leaves a gap longer than one poll; process loopback keeps running the whole
/// time (it is the fallback the stream drops back to at once).
#[derive(Debug, Default)]
pub struct Router {
	failures: u32,
	retry_at: Option<Instant>,
}

impl Router {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn step(&mut self, input: RouteInput<'_>, now: Instant) -> Decision {
		let mut capture = input.capture;
		if capture == Capture::Failed {
			self.failures = self.failures.saturating_add(1);
			let backoff = FIRST_BACKOFF
				.saturating_mul(1 << (self.failures - 1).min(5))
				.min(MAX_BACKOFF);
			self.retry_at = Some(now + backoff);
			capture = Capture::Idle;
		}
		// The endpoint we want to tap, and whether its level can be recovered.
		let target = input
			.endpoint
			.filter(|e| input.allowed && input.pad_plugged && !e.is_dualsense);
		let gain = target.and_then(compensation_gain);
		let process = |action| Decision {
			source: Source::Process,
			action,
			gain,
		};
		match capture {
			Capture::Starting(id) | Capture::Live(id)
				if target.map(|t| t.id.as_str()) != Some(id) =>
			{
				// Pad gone, default moved, or the default is now the pad: stop.
				process(Action::Stop)
			}
			Capture::Starting(_) => process(Action::None),
			Capture::Live(_) => {
				self.failures = 0;
				Decision {
					// Muted / volume 0 while it runs: keep it open (muting an open loopback is
					// safe) but stream process loopback until there is a level to recover.
					source: if gain.is_some() {
						Source::Endpoint
					} else {
						Source::Process
					},
					action: Action::None,
					gain,
				}
			}
			Capture::Idle | Capture::Failed => {
				let backing_off = self.retry_at.is_some_and(|t| now < t);
				match (target, gain) {
					// Never open into a muted endpoint: a loopback opened there can latch
					// silence (audio.md, fixed 2026-06-13).
					(Some(t), Some(_)) if !backing_off => process(Action::Start(t.id.clone())),
					_ => process(Action::None),
				}
			}
		}
	}
}

/// Smooths gain changes (a host volume step) so they don't click: a one-pole glide.
#[derive(Clone, Debug)]
pub struct GainSmoother {
	cur: f32,
	target: f32,
	coef: f32,
	primed: bool,
}

impl GainSmoother {
	/// `tau_ms`: time constant of the glide.
	pub fn new(rate: u32, tau_ms: f32) -> Self {
		let samples = (tau_ms / 1000.0 * rate as f32).max(1.0);
		Self {
			cur: 1.0,
			target: 1.0,
			coef: 1.0 - (-1.0 / samples).exp(),
			primed: false,
		}
	}

	/// The first target is taken at once (no glide from 1.0 at capture start).
	pub fn set_target(&mut self, g: f32) {
		self.target = g;
		if !self.primed {
			self.cur = g;
			self.primed = true;
		}
	}

	/// The gain for the next frame.
	pub fn next(&mut self) -> f32 {
		self.cur += (self.target - self.cur) * self.coef;
		if (self.cur - self.target).abs() <= self.target.abs() * 1e-4 {
			self.cur = self.target;
		}
		self.cur
	}

	pub fn current(&self) -> f32 {
		self.cur
	}
}

/// One interleaved frame down to stereo. 1 ch: both sides; 2: as is; 4: quad (FL FR BL
/// BR); 6/8: 5.1/7.1 in the WAVEFORMATEXTENSIBLE order (FL FR FC LFE BL BR [SL SR]), LFE
/// dropped; anything else: the first two channels.
pub fn downmix_frame(f: &[f32]) -> (f32, f32) {
	const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
	match f.len() {
		0 => (0.0, 0.0),
		1 => (f[0], f[0]),
		4 => (f[0] + H * f[2], f[1] + H * f[3]),
		6 => (f[0] + H * f[2] + H * f[4], f[1] + H * f[2] + H * f[5]),
		8 => (
			f[0] + H * f[2] + H * f[4] + H * f[6],
			f[1] + H * f[2] + H * f[5] + H * f[7],
		),
		_ => (f[0], f[1]),
	}
}

/// Endpoint loopback PCM (any channel count, f32 or s16) → stereo f32le with the
/// compensation gain applied (and clamped to ±1, so a volume step that reaches the capture
/// before the new gain does can't blast the client).
pub struct StereoConverter {
	channels: usize,
	float: bool,
	gain: GainSmoother,
	pending: Vec<u8>,
	frame: Vec<f32>,
}

impl StereoConverter {
	pub fn new(channels: u16, float: bool, rate: u32) -> Self {
		Self {
			channels: channels.max(1) as usize,
			float,
			gain: GainSmoother::new(rate, 10.0),
			pending: Vec::new(),
			frame: vec![0.0; channels.max(1) as usize],
		}
	}

	pub fn set_gain(&mut self, g: f32) {
		self.gain.set_target(g);
	}

	pub fn gain(&self) -> f32 {
		self.gain.current()
	}

	/// Append the stereo f32le of every whole frame in `pending + input` to `out`.
	pub fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) {
		let bps = if self.float { 4 } else { 2 };
		let fb = self.channels * bps;
		self.pending.extend_from_slice(input);
		let whole = self.pending.len() / fb * fb;
		out.reserve(whole / fb * 8);
		for fr in self.pending[..whole].chunks_exact(fb) {
			for (dst, b) in self.frame.iter_mut().zip(fr.chunks_exact(bps)) {
				*dst = if self.float {
					f32::from_le_bytes([b[0], b[1], b[2], b[3]])
				} else {
					i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
				};
			}
			let (l, r) = downmix_frame(&self.frame);
			let g = self.gain.next();
			out.extend_from_slice(&(l * g).clamp(-1.0, 1.0).to_le_bytes());
			out.extend_from_slice(&(r * g).clamp(-1.0, 1.0).to_le_bytes());
		}
		self.pending.drain(..whole);
	}
}

/// Fades between the two sources at a switch so the cut doesn't click: the old source fades
/// out over 5 ms, then the new one fades in from 0. If the old source has stopped writing,
/// the switch is forced once the new one has offered 30 ms. Interleaved stereo f32.
#[derive(Clone, Debug)]
pub struct SwitchFader {
	active: Source,
	next: Option<Source>,
	gain: f32,
	step: f32,
	waited: usize,
	force_after: usize,
}

impl SwitchFader {
	pub fn new(rate: u32, start: Source) -> Self {
		Self {
			active: start,
			next: None,
			gain: 1.0,
			step: 1.0 / (rate as f32 * 0.005).max(1.0),
			waited: 0,
			force_after: (rate as usize * 30) / 1000,
		}
	}

	/// The source passing now (the old one until its fade-out ends).
	pub fn active(&self) -> Source {
		self.active
	}

	/// Where the stream is headed (`active` once a switch is done).
	pub fn wanted(&self) -> Source {
		self.next.unwrap_or(self.active)
	}

	pub fn switch_to(&mut self, s: Source) {
		if s == self.active {
			self.next = None; // a switch called off mid-fade: fade back in
		} else if self.next != Some(s) {
			self.next = Some(s);
			self.waited = 0;
		}
	}

	/// One write from `src`: scales `frames` in place; `true` = pass them on, `false` = drop.
	pub fn process(&mut self, src: Source, frames: &mut [f32]) -> bool {
		if src == self.active {
			let fading_out = self.next.is_some();
			for fr in frames.chunks_exact_mut(2) {
				if fading_out {
					self.gain -= self.step;
					if self.gain < self.step * 0.5 {
						self.gain = 0.0;
					}
				} else if self.gain < 1.0 {
					self.gain = (self.gain + self.step).min(1.0);
				}
				fr[0] *= self.gain;
				fr[1] *= self.gain;
			}
			if fading_out && self.gain == 0.0 {
				self.active = self.next.take().unwrap_or(self.active);
			}
			return true;
		}
		if self.next == Some(src) {
			self.waited += frames.len() / 2;
			if self.waited >= self.force_after {
				// The old source went quiet mid-fade: take over, fading in.
				self.active = src;
				self.next = None;
				self.gain = 0.0;
				return self.process(src, frames);
			}
		}
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn obs(id: &str, level_db: f32) -> EndpointObservation {
		EndpointObservation {
			id: id.into(),
			is_dualsense: false,
			muted: false,
			level_db,
			scalar: 0.5,
			hw_volume: false,
		}
	}

	#[test]
	fn gain_undoes_the_master_attenuation() {
		let g = compensation_gain(&obs("a", -20.0)).unwrap();
		assert!((g - 10.0).abs() < 1e-3, "{g}");
		assert_eq!(compensation_gain(&obs("a", 0.0)), Some(1.0));
		// A captured sample at −20 dB comes back at the original level.
		let captured = 0.5 * 10f32.powf(-20.0 / 20.0);
		assert!((captured * g - 0.5).abs() < 1e-5);
	}

	#[test]
	fn nothing_to_recover_means_no_gain() {
		assert_eq!(
			compensation_gain(&EndpointObservation {
				muted: true,
				..obs("a", -6.0)
			}),
			None
		);
		assert_eq!(
			compensation_gain(&EndpointObservation {
				scalar: 0.0,
				..obs("a", -65.0)
			}),
			None
		);
		assert_eq!(
			compensation_gain(&obs("a", -(MAX_COMPENSATION_DB + 1.0))),
			None
		);
		assert_eq!(compensation_gain(&obs("a", f32::NEG_INFINITY)), None);
		// Hardware volume: the loopback is pre-volume already.
		assert_eq!(
			compensation_gain(&EndpointObservation {
				hw_volume: true,
				..obs("a", -30.0)
			}),
			Some(1.0)
		);
	}

	#[test]
	fn dualsense_names() {
		assert!(is_dualsense_name(&[
			Some("Speakers (2- DualSense Wireless Controller)"),
			None
		]));
		assert!(is_dualsense_name(&[
			None,
			None,
			Some("DUALSENSE wireless controller")
		]));
		assert!(!is_dualsense_name(&[
			Some("Speakers (Realtek(R) Audio)"),
			None,
			None
		]));
		assert!(!is_dualsense_name::<&str>(&[None, None, None]));
	}

	fn step(
		r: &mut Router,
		pad: bool,
		ep: Option<&EndpointObservation>,
		cap: Capture<'_>,
		t: Instant,
	) -> Decision {
		r.step(
			RouteInput {
				pad_plugged: pad,
				allowed: true,
				endpoint: ep,
				capture: cap,
			},
			t,
		)
	}

	#[test]
	fn no_pad_stays_on_process_loopback() {
		let mut r = Router::new();
		let t = Instant::now();
		let e = obs("spk", -10.0);
		let d = step(&mut r, false, Some(&e), Capture::Idle, t);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
		// Not allowed (redirect / co-op): same even with a pad.
		let d = r.step(
			RouteInput {
				pad_plugged: true,
				allowed: false,
				endpoint: Some(&e),
				capture: Capture::Idle,
			},
			t,
		);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
	}

	#[test]
	fn pad_switches_to_the_endpoint_once_it_is_live() {
		let mut r = Router::new();
		let t = Instant::now();
		let e = obs("spk", -14.0);
		let d = step(&mut r, true, Some(&e), Capture::Idle, t);
		assert_eq!(
			(d.source, d.action),
			(Source::Process, Action::Start("spk".into()))
		);
		// Starting: still process loopback (no gap).
		let d = step(&mut r, true, Some(&e), Capture::Starting("spk"), t);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
		let d = step(&mut r, true, Some(&e), Capture::Live("spk"), t);
		assert_eq!(
			(d.source, d.action.clone()),
			(Source::Endpoint, Action::None)
		);
		assert!((d.gain.unwrap() - 10f32.powf(14.0 / 20.0)).abs() < 1e-3);
		// Pad unplugged: back to process loopback, capture stopped.
		let d = step(&mut r, false, Some(&e), Capture::Live("spk"), t);
		assert_eq!((d.source, d.action), (Source::Process, Action::Stop));
	}

	#[test]
	fn mute_falls_back_and_unmute_returns_without_reopening() {
		let mut r = Router::new();
		let t = Instant::now();
		let muted = EndpointObservation {
			muted: true,
			..obs("spk", -10.0)
		};
		// Muted before the capture opened: never opened into the mute.
		let d = step(&mut r, true, Some(&muted), Capture::Idle, t);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
		// Muted while live: kept open, process loopback streams.
		let d = step(&mut r, true, Some(&muted), Capture::Live("spk"), t);
		assert_eq!(
			(d.source, d.action, d.gain),
			(Source::Process, Action::None, None)
		);
		let zero = EndpointObservation {
			scalar: 0.0,
			..obs("spk", -65.0)
		};
		assert_eq!(
			step(&mut r, true, Some(&zero), Capture::Live("spk"), t).source,
			Source::Process
		);
		let e = obs("spk", -10.0);
		assert_eq!(
			step(&mut r, true, Some(&e), Capture::Live("spk"), t).source,
			Source::Endpoint
		);
	}

	#[test]
	fn default_change_restarts_and_dualsense_default_falls_back() {
		let mut r = Router::new();
		let t = Instant::now();
		let hp = obs("headphones", -3.0);
		let d = step(&mut r, true, Some(&hp), Capture::Live("spk"), t);
		assert_eq!((d.source, d.action), (Source::Process, Action::Stop));
		let d = step(&mut r, true, Some(&hp), Capture::Idle, t);
		assert_eq!(d.action, Action::Start("headphones".into()));
		// The pad's own endpoint became the default.
		let pad = EndpointObservation {
			is_dualsense: true,
			..obs("pad", 0.0)
		};
		let d = step(&mut r, true, Some(&pad), Capture::Starting("headphones"), t);
		assert_eq!((d.source, d.action), (Source::Process, Action::Stop));
		let d = step(&mut r, true, Some(&pad), Capture::Idle, t);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
		// No default endpoint at all.
		let d = step(&mut r, true, None, Capture::Live("headphones"), t);
		assert_eq!((d.source, d.action), (Source::Process, Action::Stop));
	}

	#[test]
	fn a_failed_capture_backs_off_before_retrying() {
		let mut r = Router::new();
		let t = Instant::now();
		let e = obs("spk", -10.0);
		let d = step(&mut r, true, Some(&e), Capture::Failed, t);
		assert_eq!((d.source, d.action), (Source::Process, Action::None));
		assert_eq!(
			step(
				&mut r,
				true,
				Some(&e),
				Capture::Idle,
				t + Duration::from_secs(1)
			)
			.action,
			Action::None
		);
		let d = step(
			&mut r,
			true,
			Some(&e),
			Capture::Idle,
			t + Duration::from_secs(2),
		);
		assert_eq!(d.action, Action::Start("spk".into()));
		// Second failure in a row: 4 s.
		step(
			&mut r,
			true,
			Some(&e),
			Capture::Failed,
			t + Duration::from_secs(3),
		);
		assert_eq!(
			step(
				&mut r,
				true,
				Some(&e),
				Capture::Idle,
				t + Duration::from_secs(6)
			)
			.action,
			Action::None
		);
		assert_eq!(
			step(
				&mut r,
				true,
				Some(&e),
				Capture::Idle,
				t + Duration::from_secs(7)
			)
			.action,
			Action::Start("spk".into())
		);
	}

	#[test]
	fn smoother_glides_without_a_step() {
		let mut s = GainSmoother::new(48_000, 10.0);
		s.set_target(4.0);
		assert_eq!(s.next(), 4.0, "first target is immediate");
		s.set_target(1.0);
		let mut prev = s.current();
		let mut max_step: f32 = 0.0;
		for _ in 0..48_00 {
			let g = s.next();
			max_step = max_step.max((prev - g).abs());
			prev = g;
		}
		assert!(max_step < 0.01, "per-sample step {max_step}");
		assert!((prev - 1.0).abs() < 1e-3, "settled within 100 ms: {prev}");
	}

	#[test]
	fn downmix_layouts() {
		assert_eq!(downmix_frame(&[0.3]), (0.3, 0.3));
		assert_eq!(downmix_frame(&[0.1, 0.2]), (0.1, 0.2));
		let h = std::f32::consts::FRAC_1_SQRT_2;
		let (l, r) = downmix_frame(&[0.1, 0.2, 0.4, 0.9, 0.0, 0.0]);
		assert!(
			(l - (0.1 + h * 0.4)).abs() < 1e-6 && (r - (0.2 + h * 0.4)).abs() < 1e-6,
			"LFE dropped"
		);
		assert_eq!(downmix_frame(&[0.1, 0.2, 0.3]), (0.1, 0.2));
	}

	#[test]
	fn converter_applies_gain_across_odd_writes() {
		let mut c = StereoConverter::new(2, true, 48_000);
		c.set_gain(2.0);
		let mut pcm = Vec::new();
		for i in 0..10 {
			let v = 0.01 * i as f32;
			pcm.extend_from_slice(&v.to_le_bytes());
			pcm.extend_from_slice(&(-v).to_le_bytes());
		}
		let mut out = Vec::new();
		for chunk in pcm.chunks(7) {
			c.convert(chunk, &mut out);
		}
		assert_eq!(out.len(), pcm.len());
		let f = |i: usize| f32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
		assert!((f(18) - 0.18).abs() < 1e-6 && (f(19) + 0.18).abs() < 1e-6);
		// Clamped, s16 input, 6 channels.
		let mut c = StereoConverter::new(6, false, 48_000);
		c.set_gain(100.0);
		let mut out = Vec::new();
		let frame: Vec<u8> = [16384i16, -16384, 0, 0, 0, 0]
			.iter()
			.flat_map(|s| s.to_le_bytes())
			.collect();
		c.convert(&frame, &mut out);
		let f = |i: usize| f32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
		assert_eq!((f(0), f(1)), (1.0, -1.0));
	}

	#[test]
	fn switch_fades_out_then_in_without_a_step() {
		let mut f = SwitchFader::new(48_000, Source::Process);
		let mut out = Vec::new();
		// Both sources carry full-scale DC; record what passes, in order.
		let feed = |f: &mut SwitchFader, src, n: usize, out: &mut Vec<f32>| {
			let mut b = vec![1.0f32; n * 2];
			if f.process(src, &mut b) {
				out.extend(b.chunks_exact(2).map(|c| c[0]));
			}
		};
		feed(&mut f, Source::Process, 480, &mut out);
		f.switch_to(Source::Endpoint);
		assert_eq!(f.wanted(), Source::Endpoint);
		for _ in 0..4 {
			feed(&mut f, Source::Endpoint, 120, &mut out);
			feed(&mut f, Source::Process, 120, &mut out);
		}
		assert_eq!(f.active(), Source::Endpoint, "old source faded out in 5 ms");
		for _ in 0..4 {
			feed(&mut f, Source::Process, 120, &mut out);
			feed(&mut f, Source::Endpoint, 120, &mut out);
		}
		let max = out
			.windows(2)
			.map(|w| (w[0] - w[1]).abs())
			.fold(0.0, f32::max);
		assert!(max <= 1.0 / 240.0 + 1e-6, "step {max}");
		assert_eq!(*out.last().unwrap(), 1.0, "faded back in");
	}

	#[test]
	fn switch_is_forced_when_the_old_source_is_gone() {
		let mut f = SwitchFader::new(48_000, Source::Endpoint);
		f.switch_to(Source::Process);
		let mut b = vec![1.0f32; 480 * 2];
		assert!(
			!f.process(Source::Process, &mut b),
			"10 ms: still waiting for the fade-out"
		);
		let mut b = vec![1.0f32; 1000 * 2];
		assert!(f.process(Source::Process, &mut b), "30 ms: taken over");
		assert_eq!(f.active(), Source::Process);
		assert!(b[0] < 0.01 && b[b.len() - 1] == 1.0, "fades in");
	}
}
