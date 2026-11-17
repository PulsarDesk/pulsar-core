//! Voice calls over a session, like a phone: one side rings, the other answers or declines.
//! Either peer may call. The protocol lives here so every app (desktop, mobile) behaves the
//! same; the apps only carry out the [`Step`]s (send the signal, ring, open the mics, show it).
//!
//! Signalling is [`DataMsg::Call`](crate::service::DataMsg::Call) ([`CallSignal`]). The audio is
//! the mic side channel (`DataMsg::Audio`, s16le 48 kHz mono, 20 ms) in BOTH directions while a
//! call is up.
//!
//! The session transport is lossy, so: the caller repeats `Ring` every [`RING_REPEAT`] until
//! answered; a `Ring` that reaches a callee already in the call is answered with `Accept`
//! again, one that reaches it right after a decline with `Decline` again; final signals
//! (`Decline` / `Cancel` / `End`) go out [`FINAL_REPEATS`] times; and both sides time a ring out
//! on their own ([`OUT_TIMEOUT`], [`IN_TIMEOUT`]) — call [`Call::tick`] about twice a second
//! while a call is not idle.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::service::CallSignal;

/// The caller gives up after this long without an answer.
pub const OUT_TIMEOUT: Duration = Duration::from_secs(40);
/// The callee stops ringing after this long (sooner than the caller gives up).
pub const IN_TIMEOUT: Duration = Duration::from_secs(35);
/// The caller re-sends `Ring` this often while it rings.
pub const RING_REPEAT: Duration = Duration::from_secs(2);
/// How many times a final signal is sent.
pub const FINAL_REPEATS: usize = 3;
/// After a decline, a late repeated `Ring` is declined again for this long instead of ringing.
pub const DECLINE_MEMORY: Duration = Duration::from_secs(8);
/// When to say [`CallSignal::Hello`] after the session came up (repeated: datagrams drop),
/// until the peer is known to take calls.
pub const HELLO_AT: &[Duration] = &[
	Duration::ZERO,
	Duration::from_secs(1),
	Duration::from_secs(3),
	Duration::from_secs(10),
	Duration::from_secs(30),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
	Idle,
	/// We are calling; the peer's phone rings.
	Outgoing,
	/// The peer is calling us; our ringtone plays.
	Incoming,
	/// Talking (both mics on).
	Active,
}

impl Phase {
	/// Short token for line protocols (`idle` / `out` / `in` / `active`).
	pub fn token(self) -> &'static str {
		match self {
			Phase::Idle => "idle",
			Phase::Outgoing => "out",
			Phase::Incoming => "in",
			Phase::Active => "active",
		}
	}
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
	/// We hung up / declined ourselves.
	ByUs,
	/// The peer declined our call.
	Declined,
	/// Our call rang out.
	NoAnswer,
	/// The peer's call rang out or was cancelled before we answered.
	Missed,
	/// The peer ended an answered call.
	HungUp,
	/// The session went away.
	Gone,
}

impl Ended {
	/// The notice for an end the user did not cause (`call-missed`, `call-declined`,
	/// `call-noanswer`, `call-ended`); `None` when there is nothing to tell.
	pub fn notice(self) -> Option<&'static str> {
		match self {
			Ended::Declined => Some("call-declined"),
			Ended::NoAnswer => Some("call-noanswer"),
			Ended::Missed => Some("call-missed"),
			Ended::HungUp => Some("call-ended"),
			Ended::ByUs | Ended::Gone => None,
		}
	}
}

/// What the local user did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
	/// Ring the peer (on an incoming call: answer it).
	Start,
	Accept,
	Decline,
	/// Cancel our ring, decline theirs, or end the call.
	Hangup,
}

impl Action {
	pub fn parse(s: &str) -> Option<Action> {
		match s {
			"start" => Some(Action::Start),
			"accept" => Some(Action::Accept),
			"decline" => Some(Action::Decline),
			"hangup" => Some(Action::Hangup),
			_ => None,
		}
	}
}

/// What the app must do after an event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
	/// Send this signal to the peer, this many times.
	pub send: Option<(CallSignal, usize)>,
	/// The phase changed: (from, to).
	pub moved: Option<(Phase, Phase)>,
	/// Why the call ended (set when it moved to `Idle`).
	pub ended: Option<Ended>,
}

impl Step {
	fn send(sig: CallSignal, times: usize) -> Step {
		Step { send: Some((sig, times)), ..Step::default() }
	}
}

/// One session's call.
#[derive(Clone, Debug)]
pub struct Call {
	phase: Phase,
	/// The peer said it takes calls (`Hello`, or any call signal).
	supported: bool,
	since: Instant,
	last_ring: Instant,
	declined_at: Option<Instant>,
}

impl Default for Call {
	fn default() -> Self {
		Call::new()
	}
}

impl Call {
	pub fn new() -> Call {
		let now = Instant::now();
		Call { phase: Phase::Idle, supported: false, since: now, last_ring: now, declined_at: None }
	}

	pub fn phase(&self) -> Phase {
		self.phase
	}

	/// Whether the peer takes calls (offer calling only then: an older peer never answers).
	pub fn supported(&self) -> bool {
		self.supported
	}

	/// When the current phase began.
	pub fn since(&self) -> Instant {
		self.since
	}

	fn go(&mut self, to: Phase, now: Instant, ended: Option<Ended>, mut step: Step) -> Step {
		let from = self.phase;
		self.phase = to;
		self.since = now;
		step.moved = Some((from, to));
		step.ended = ended;
		step
	}

	/// The local user acted.
	pub fn action(&mut self, a: Action, now: Instant) -> Step {
		match (a, self.phase) {
			(Action::Start, Phase::Idle) => {
				self.last_ring = now;
				self.go(Phase::Outgoing, now, None, Step::send(CallSignal::Ring, 1))
			}
			(Action::Accept | Action::Start, Phase::Incoming) => {
				self.go(Phase::Active, now, None, Step::send(CallSignal::Accept, FINAL_REPEATS))
			}
			(Action::Decline | Action::Hangup, Phase::Incoming) => {
				self.declined_at = Some(now);
				self.go(Phase::Idle, now, Some(Ended::ByUs), Step::send(CallSignal::Decline, FINAL_REPEATS))
			}
			(Action::Hangup | Action::Decline, Phase::Outgoing) => {
				self.go(Phase::Idle, now, Some(Ended::ByUs), Step::send(CallSignal::Cancel, FINAL_REPEATS))
			}
			(Action::Hangup, Phase::Active) => {
				self.go(Phase::Idle, now, Some(Ended::ByUs), Step::send(CallSignal::End, FINAL_REPEATS))
			}
			_ => Step::default(),
		}
	}

	/// A signal from the peer. The first `Hello` is answered with one (ours may have been lost).
	pub fn signal(&mut self, s: CallSignal, now: Instant) -> Step {
		let first = !self.supported;
		self.supported = true;
		if s == CallSignal::Hello {
			return if first { Step::send(CallSignal::Hello, 1) } else { Step::default() };
		}
		let declined_recently = self.declined_at.is_some_and(|t| now.saturating_duration_since(t) < DECLINE_MEMORY);
		match (s, self.phase) {
			(CallSignal::Ring, Phase::Idle) if declined_recently => Step::send(CallSignal::Decline, 1),
			(CallSignal::Ring, Phase::Idle) => self.go(Phase::Incoming, now, None, Step::default()),
			// Both rang at once (both wanted to talk), or the peer answered.
			(CallSignal::Ring | CallSignal::Accept, Phase::Outgoing) => {
				self.go(Phase::Active, now, None, Step::send(CallSignal::Accept, FINAL_REPEATS))
			}
			// Our Accept was lost: the caller still rings.
			(CallSignal::Ring, Phase::Active) => Step::send(CallSignal::Accept, 1),
			(CallSignal::Decline, Phase::Outgoing) => self.go(Phase::Idle, now, Some(Ended::Declined), Step::default()),
			(CallSignal::Cancel, Phase::Incoming) => self.go(Phase::Idle, now, Some(Ended::Missed), Step::default()),
			(CallSignal::End | CallSignal::Cancel | CallSignal::Decline, Phase::Active) => {
				self.go(Phase::Idle, now, Some(Ended::HungUp), Step::default())
			}
			_ => Step::default(),
		}
	}

	/// Time passing: repeat the ring, time a ring out. Call about twice a second while not idle.
	pub fn tick(&mut self, now: Instant) -> Step {
		let in_phase = now.saturating_duration_since(self.since);
		match self.phase {
			Phase::Outgoing if in_phase >= OUT_TIMEOUT => {
				self.go(Phase::Idle, now, Some(Ended::NoAnswer), Step::send(CallSignal::Cancel, FINAL_REPEATS))
			}
			Phase::Outgoing if now.saturating_duration_since(self.last_ring) >= RING_REPEAT => {
				self.last_ring = now;
				Step::send(CallSignal::Ring, 1)
			}
			Phase::Incoming if in_phase >= IN_TIMEOUT => {
				self.declined_at = Some(now);
				self.go(Phase::Idle, now, Some(Ended::Missed), Step::send(CallSignal::Decline, FINAL_REPEATS))
			}
			_ => Step::default(),
		}
	}

	/// The session ended: drop the call without signalling.
	pub fn gone(&mut self, now: Instant) -> Step {
		if self.phase == Phase::Idle {
			return Step::default();
		}
		self.go(Phase::Idle, now, Some(Ended::Gone), Step::default())
	}
}

/// Built-in ringtones (synthesized, no audio assets), as the settings name them. `none` is
/// silent; apps add their own extra choices (a custom file, the system ringtone).
pub const TONES: &[&str] = &["classic", "chime", "pulse", "none"];

const RATE: f32 = 48_000.0;

/// One cycle of a built-in tone as s16le 48 kHz mono, the silence after the sound included
/// (loop it to ring). Empty for `none` / an unknown name.
pub fn tone_pcm(tone: &str) -> Vec<u8> {
	use std::f32::consts::TAU;
	let mut s: Vec<f32> = Vec::new();
	match tone {
		// Double ring (two mixed tones, 400 + 450 Hz): ring, short gap, ring, long pause.
		"classic" => {
			for (on, off) in [(0.4, 0.2), (0.4, 2.0)] {
				let start = s.len();
				s.extend((0..secs(on)).map(|i| {
					let t = i as f32 / RATE;
					0.17 * ((TAU * 400.0 * t).sin() + (TAU * 450.0 * t).sin())
				}));
				ramp(&mut s[start..], 0.008);
				silence(&mut s, off);
			}
		}
		// Three rising bell notes (G5 C6 E6) with a soft decay, twice, then a pause.
		"chime" => {
			let notes = [783.99f32, 1046.5, 1318.51];
			let step = secs(0.16);
			let len = secs(0.9);
			let mut buf = vec![0f32; step * (notes.len() - 1) + len];
			for (k, f) in notes.iter().enumerate() {
				for i in 0..len {
					let t = i as f32 / RATE;
					let env = (-t / 0.28).exp() * (t / 0.004).min(1.0);
					let v = (TAU * f * t).sin() + 0.25 * (TAU * 2.0 * f * t).sin();
					buf[k * step + i] += 0.22 * env * v;
				}
			}
			s.extend_from_slice(&buf);
			silence(&mut s, 0.25);
			s.extend_from_slice(&buf);
			silence(&mut s, 1.6);
		}
		// Three short soft beeps (880 Hz), then a pause.
		"pulse" => {
			for _ in 0..3 {
				let start = s.len();
				s.extend((0..secs(0.12)).map(|i| 0.3 * (TAU * 880.0 * i as f32 / RATE).sin()));
				ramp(&mut s[start..], 0.01);
				silence(&mut s, 0.1);
			}
			silence(&mut s, 1.4);
		}
		_ => {}
	}
	s.iter().flat_map(|v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect()
}

fn secs(x: f32) -> usize {
	(x * RATE) as usize
}

fn silence(s: &mut Vec<f32>, x: f32) {
	s.extend(std::iter::repeat_n(0.0, secs(x)));
}

/// Fade both ends over `x` seconds (no clicks).
fn ramp(s: &mut [f32], x: f32) {
	let n = secs(x).min(s.len() / 2).max(1);
	let len = s.len();
	for i in 0..n {
		let g = i as f32 / n as f32;
		s[i] *= g;
		s[len - 1 - i] *= g;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use CallSignal::*;

	fn moved(s: Step) -> Option<Phase> {
		s.moved.map(|(_, to)| to)
	}

	#[test]
	fn a_call_rings_is_answered_and_hung_up() {
		let t0 = Instant::now();
		let (mut a, mut b) = (Call::new(), Call::new());
		let s = a.action(Action::Start, t0);
		assert_eq!((s.send, moved(s)), (Some((Ring, 1)), Some(Phase::Outgoing)));
		let s = b.signal(Ring, t0);
		assert_eq!((s.send, moved(s)), (None, Some(Phase::Incoming)));
		let s = b.action(Action::Accept, t0);
		assert_eq!((s.send, moved(s)), (Some((Accept, FINAL_REPEATS)), Some(Phase::Active)));
		let s = a.signal(Accept, t0);
		assert_eq!(moved(s), Some(Phase::Active));
		// Repeated Accepts (sent 3×) are ignored once active.
		assert_eq!(a.signal(Accept, t0), Step::default());
		let s = b.action(Action::Hangup, t0);
		assert_eq!((s.send, s.ended), (Some((End, FINAL_REPEATS)), Some(Ended::ByUs)));
		let s = a.signal(End, t0);
		assert_eq!((moved(s), s.ended), (Some(Phase::Idle), Some(Ended::HungUp)));
		assert_eq!(Ended::HungUp.notice(), Some("call-ended"));
	}

	#[test]
	fn the_caller_repeats_the_ring_and_gives_up() {
		let t0 = Instant::now();
		let mut a = Call::new();
		a.action(Action::Start, t0);
		assert_eq!(a.tick(t0 + Duration::from_millis(500)), Step::default());
		assert_eq!(a.tick(t0 + RING_REPEAT).send, Some((Ring, 1)));
		assert_eq!(a.tick(t0 + RING_REPEAT + Duration::from_millis(100)).send, None);
		let s = a.tick(t0 + OUT_TIMEOUT);
		assert_eq!((s.send, s.ended), (Some((Cancel, FINAL_REPEATS)), Some(Ended::NoAnswer)));
		assert_eq!(a.phase(), Phase::Idle);
	}

	#[test]
	fn the_callee_stops_ringing_and_declines_late_rings() {
		let t0 = Instant::now();
		let mut b = Call::new();
		b.signal(Ring, t0);
		let s = b.tick(t0 + IN_TIMEOUT);
		assert_eq!((s.send, s.ended), (Some((Decline, FINAL_REPEATS)), Some(Ended::Missed)));
		// The caller's next repeated ring must not ring again.
		let s = b.signal(Ring, t0 + IN_TIMEOUT + Duration::from_secs(1));
		assert_eq!((s.send, s.moved), (Some((Decline, 1)), None));
		// Much later it is a new call.
		let s = b.signal(Ring, t0 + IN_TIMEOUT + DECLINE_MEMORY + Duration::from_secs(1));
		assert_eq!(moved(s), Some(Phase::Incoming));
	}

	#[test]
	fn lost_signals_are_recovered() {
		let t0 = Instant::now();
		let mut b = Call::new();
		b.signal(Ring, t0);
		b.action(Action::Accept, t0);
		// Our Accepts were all lost: the caller rings again → answer again.
		assert_eq!(b.signal(Ring, t0).send, Some((Accept, 1)));
		// A Cancel crossing our answer ends the call.
		assert_eq!(b.signal(Cancel, t0).ended, Some(Ended::HungUp));
	}

	#[test]
	fn hello_marks_support_and_is_answered_once() {
		let t0 = Instant::now();
		let mut c = Call::new();
		assert!(!c.supported());
		assert_eq!(c.signal(Hello, t0).send, Some((Hello, 1)));
		assert!(c.supported());
		assert_eq!(c.signal(Hello, t0), Step::default());
		// Any call signal proves support too.
		let mut d = Call::new();
		d.signal(Ring, t0);
		assert!(d.supported());
	}

	#[test]
	fn crossing_rings_connect() {
		let t0 = Instant::now();
		let mut a = Call::new();
		a.action(Action::Start, t0);
		let s = a.signal(Ring, t0);
		assert_eq!((s.send, moved(s)), (Some((Accept, FINAL_REPEATS)), Some(Phase::Active)));
	}

	#[test]
	fn cancel_and_decline() {
		let t0 = Instant::now();
		let (mut a, mut b) = (Call::new(), Call::new());
		a.action(Action::Start, t0);
		b.signal(Ring, t0);
		assert_eq!(a.action(Action::Hangup, t0).send, Some((Cancel, FINAL_REPEATS)));
		assert_eq!(b.signal(Cancel, t0).ended, Some(Ended::Missed));
		a.action(Action::Start, t0);
		b.signal(Ring, t0 + DECLINE_MEMORY);
		assert_eq!(b.action(Action::Decline, t0).send, Some((Decline, FINAL_REPEATS)));
		assert_eq!(a.signal(Decline, t0).ended, Some(Ended::Declined));
		// Nothing to do when idle.
		assert_eq!(Call::new().action(Action::Hangup, t0), Step::default());
		assert_eq!(Call::new().gone(t0), Step::default());
		assert_eq!(Action::parse("accept"), Some(Action::Accept));
		assert_eq!(Action::parse("x"), None);
	}

	#[test]
	fn built_in_tones_have_sound_and_end_quiet() {
		for t in ["classic", "chime", "pulse"] {
			let pcm = tone_pcm(t);
			assert!(pcm.len() > 96 * 1500, "{t}: at least 1.5 s");
			let v: Vec<i16> = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
			assert!(v.iter().map(|x| x.unsigned_abs()).max().unwrap() > 5000, "{t}: audible");
			assert!(v[v.len() - 1].unsigned_abs() < 50, "{t}: ends in silence");
		}
		assert!(tone_pcm("none").is_empty());
	}
}
