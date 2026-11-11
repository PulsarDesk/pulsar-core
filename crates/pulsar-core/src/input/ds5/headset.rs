//! Client side: play the session's audio through the pad's headset jack ("PS5 gibi").
//!
//! When headphones are plugged into the client's DualSense, the host's normal audio (what
//! the client would play on its own output) goes to the pad instead, mixed with the game's
//! own pad-speaker stream, which a real pad also plays on the headphones when they are in.
//! Unplugged, everything goes back to the client output.
//!
//! Pure parts only (the app owns the codecs, the audio device and the pad thread):
//! - [`JackSwitch`]: debounced jack state + the routing decision.
//! - [`HeadsetMix`]: pairs session audio blocks with host pad slots (haptics + decoded
//!   pad-speaker PCM) into pad slots on the session audio's clock.
//!
//! Policy (maintainer, 2026-11-11): while the headset route is active Pulsar makes no sound
//! anywhere else: the client output is silent and nothing plays on the pad's built-in
//! speaker. If the route has to stop while the headphones are still in (setting changed,
//! auto-degrade shed it as the very last feature), the session audio goes silent; it never
//! moves back to the client output until the headphones are unplugged.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::haptics::Frame;
use super::report::Jack;
use super::speaker::{SpeakerFramer, OUT_FRAMES};

/// The jack must read the same for this long before a change counts (contacts bounce
/// while a plug goes in).
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// What the app has to do after a [`JackSwitch::update`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
	/// Headphones were plugged in (debounced): route the pad's speaker stream to the jack.
	Plugged,
	/// Headphones were pulled out.
	Unplugged,
	/// Session audio now plays through the pad's jack: silence the client output.
	RouteOn,
	/// Session audio stopped going to the pad. `restore` = the client output may play again
	/// (the headphones are out); false = stay silent (headphones still in, route not allowed).
	RouteOff { restore: bool },
}

/// Debounced jack state and the session-audio routing decision for one pad.
#[derive(Debug, Default)]
pub struct JackSwitch {
	/// Debounced headphone state.
	plugged: bool,
	/// Raw reading being confirmed, since when.
	candidate: Option<(bool, Instant)>,
	/// Session audio goes to this pad.
	routed: bool,
	/// The client output must stay silent (routed, or plugged with the route not allowed).
	silenced: bool,
}

impl JackSwitch {
	pub fn plugged(&self) -> bool {
		self.plugged
	}

	pub fn routed(&self) -> bool {
		self.routed
	}

	/// Whether the client's own output must be silent because of this pad.
	pub fn silenced(&self) -> bool {
		self.silenced
	}

	/// Feed the latest jack reading (`None` = no new input report), what the settings and
	/// auto-degrade allow ([`Route`]) and whether this pad may carry the session's audio
	/// (`claimable` = no other pad already does). Returns the switches, in order.
	pub fn update(
		&mut self,
		jack: Option<Jack>,
		route: Route,
		claimable: bool,
		now: Instant,
	) -> Vec<Switch> {
		let mut out = Vec::new();
		if let Some(j) = jack {
			if j.headphones == self.plugged {
				self.candidate = None;
			} else {
				match self.candidate {
					Some((v, t)) if v == j.headphones => {
						if now.saturating_duration_since(t) >= DEBOUNCE {
							self.plugged = j.headphones;
							self.candidate = None;
							out.push(if self.plugged {
								Switch::Plugged
							} else {
								Switch::Unplugged
							});
						}
					}
					_ => self.candidate = Some((j.headphones, now)),
				}
			}
		}
		let want = self.plugged && route == Route::Allowed && (self.routed || claimable);
		// Kapalı = the feature is off and the client output plays as usual; shed by
		// auto-degrade = silent while the headphones are in (module docs).
		let silenced = want || (self.plugged && route == Route::Shed);
		if want != self.routed {
			self.routed = want;
			out.push(if want {
				Switch::RouteOn
			} else {
				Switch::RouteOff { restore: !silenced }
			});
		} else if self.silenced && !silenced {
			out.push(Switch::RouteOff { restore: true });
		}
		self.silenced = silenced;
		out
	}
}

/// What the settings and auto-degrade allow for the headset route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
	/// Otomatik / Açık and not shed.
	Allowed,
	/// Otomatik, shed by auto-degrade: no headset audio, and the client stays silent.
	Shed,
	/// Kapalı: the client output plays as without this feature.
	Off,
}

/// One slot for the pad: haptics (or none) and stereo speaker PCM (480 frames, the pad's
/// squeezed 10 ms block) to encode, or none.
#[derive(Debug, Clone, PartialEq)]
pub struct MixedSlot {
	pub haptics: Option<Frame>,
	pub pcm: Option<Vec<f32>>,
}

/// Host slots held back waiting for session audio before they go out alone (the session
/// stream paused or runs behind): 4 slots = 43 ms.
pub const HOST_LAG: usize = 4;
/// Session blocks queued before the oldest is dropped (≈ 85 ms).
const MAX_SESSION: usize = 8;

/// Pairs session audio with host pad slots (module docs). Session audio is the clock: each
/// complete 512-frame block (squeezed to 480 like the pad speaker stream) becomes one pad
/// slot carrying the next host slot's haptics, its pad-speaker PCM mixed in. Host slots
/// without session audio go out alone once [`HOST_LAG`] of them are waiting, so haptics
/// never stall when the game is silent.
#[derive(Debug, Default)]
pub struct HeadsetMix {
	framer: SpeakerFramer,
	session: VecDeque<Vec<f32>>,
	host: VecDeque<(Option<Frame>, Option<Vec<f32>>)>,
	/// Fade-in ramp position for the first session blocks after a (re)start, in frames.
	ramp: usize,
	pub dropped: u64,
	blocks: Vec<Vec<f32>>,
}

/// Fade-in length after the route turns on (no click), in frames (5 ms).
const RAMP_FRAMES: usize = 240;

impl HeadsetMix {
	/// Session PCM, 48 kHz interleaved stereo.
	pub fn push_session(&mut self, stereo: &[f32]) {
		self.framer.push(stereo, &mut self.blocks);
		for mut b in self.blocks.drain(..) {
			while self.ramp < RAMP_FRAMES {
				let i = self.ramp;
				if i >= OUT_FRAMES {
					break;
				}
				let g = i as f32 / RAMP_FRAMES as f32;
				b[i * 2] *= g;
				b[i * 2 + 1] *= g;
				self.ramp += 1;
			}
			if self.session.len() >= MAX_SESSION {
				self.session.pop_front();
				self.dropped += 1;
			}
			self.session.push_back(b);
		}
	}

	/// A host slot: haptics and the decoded pad-speaker PCM (480 stereo frames).
	pub fn push_host(&mut self, haptics: Option<Frame>, speaker: Option<Vec<f32>>) {
		self.host.push_back((haptics, speaker));
	}

	/// Drop everything queued and start over with a fade-in (route turned on or off).
	pub fn reset(&mut self) {
		*self = Self {
			dropped: self.dropped,
			..Self::default()
		};
	}

	/// Slots ready for the pad, oldest first.
	pub fn pop(&mut self, out: &mut Vec<MixedSlot>) {
		while let Some(mut s) = self.session.pop_front() {
			let (haptics, host) = self.host.pop_front().unwrap_or((None, None));
			if let Some(h) = host {
				mix_into(&mut s, &h);
			}
			out.push(MixedSlot {
				haptics,
				pcm: Some(s),
			});
		}
		while self.host.len() > HOST_LAG {
			let (haptics, pcm) = self.host.pop_front().expect("len checked");
			if haptics.is_some() || pcm.is_some() {
				out.push(MixedSlot { haptics, pcm });
			}
		}
	}
}

/// `dst += src`, clamped to [-1, 1] (a hard clip only on a real overload; both streams are
/// game mixes that already leave headroom).
pub fn mix_into(dst: &mut [f32], src: &[f32]) {
	for (d, s) in dst.iter_mut().zip(src) {
		*d = (*d + *s).clamp(-1.0, 1.0);
	}
}

#[cfg(test)]
mod tests {
	use super::super::speaker::IN_FRAMES;
	use super::*;

	const HP: Jack = Jack {
		headphones: true,
		mic: false,
	};
	const NONE: Jack = Jack {
		headphones: false,
		mic: false,
	};

	fn ms(t0: Instant, n: u64) -> Instant {
		t0 + Duration::from_millis(n)
	}

	#[test]
	fn plug_is_debounced_then_routes() {
		let t0 = Instant::now();
		let mut j = JackSwitch::default();
		assert!(j.update(Some(HP), Route::Allowed, true, t0).is_empty());
		// A bounce resets the timer.
		assert!(j
			.update(Some(NONE), Route::Allowed, true, ms(t0, 50))
			.is_empty());
		assert!(j
			.update(Some(HP), Route::Allowed, true, ms(t0, 60))
			.is_empty());
		assert!(j
			.update(Some(HP), Route::Allowed, true, ms(t0, 200))
			.is_empty());
		assert!(!j.silenced());
		assert_eq!(
			j.update(Some(HP), Route::Allowed, true, ms(t0, 210)),
			vec![Switch::Plugged, Switch::RouteOn]
		);
		assert!(j.plugged() && j.routed() && j.silenced());
		// Unplug: back to the client output.
		j.update(Some(NONE), Route::Allowed, true, ms(t0, 300));
		assert_eq!(
			j.update(None, Route::Allowed, true, ms(t0, 400)),
			vec![],
			"no new reading: no change"
		);
		assert_eq!(
			j.update(Some(NONE), Route::Allowed, true, ms(t0, 460)),
			vec![Switch::Unplugged, Switch::RouteOff { restore: true }]
		);
		assert!(!j.silenced());
	}

	fn plugged(route: Route, claimable: bool) -> JackSwitch {
		let t0 = Instant::now();
		let mut j = JackSwitch::default();
		j.update(Some(HP), route, claimable, t0);
		j.update(Some(HP), route, claimable, ms(t0, 200));
		assert!(j.plugged());
		j
	}

	#[test]
	fn shed_goes_silent_and_never_back_to_the_client() {
		let mut j = plugged(Route::Allowed, true);
		let now = Instant::now();
		assert_eq!(
			j.update(None, Route::Shed, true, now),
			vec![Switch::RouteOff { restore: false }]
		);
		assert!(j.silenced() && !j.routed());
		// Restored by auto-degrade.
		assert_eq!(
			j.update(None, Route::Allowed, true, now),
			vec![Switch::RouteOn]
		);
		// Shed again, then the headphones come out: the client output comes back.
		j.update(None, Route::Shed, true, now);
		j.update(Some(NONE), Route::Shed, true, now);
		assert_eq!(
			j.update(Some(NONE), Route::Shed, true, now + DEBOUNCE),
			vec![Switch::Unplugged, Switch::RouteOff { restore: true }]
		);
		assert!(!j.silenced());
	}

	#[test]
	fn off_setting_leaves_the_client_output_alone() {
		let j = plugged(Route::Off, true);
		assert!(!j.routed() && !j.silenced());
		// Turned off while routed: the client output plays again.
		let mut j = plugged(Route::Allowed, true);
		assert_eq!(
			j.update(None, Route::Off, true, Instant::now()),
			vec![Switch::RouteOff { restore: true }]
		);
		assert!(!j.silenced());
		// Shed, then the user turns it off: the client output is released too.
		let mut j = plugged(Route::Shed, true);
		assert!(j.silenced());
		assert_eq!(
			j.update(None, Route::Off, true, Instant::now()),
			vec![Switch::RouteOff { restore: true }]
		);
	}

	#[test]
	fn a_second_pad_does_not_claim_the_session_audio() {
		let j = plugged(Route::Allowed, false);
		assert!(j.plugged() && !j.routed());
		assert!(
			!j.silenced(),
			"the pad that has the route silences the client"
		);
	}

	fn session_block(v: f32) -> Vec<f32> {
		vec![v; IN_FRAMES * 2]
	}

	#[test]
	fn session_is_the_clock_and_host_audio_is_mixed_in() {
		let mut m = HeadsetMix::default();
		let mut out = Vec::new();
		m.push_host(Some([3; 64]), Some(vec![0.25; OUT_FRAMES * 2]));
		m.pop(&mut out);
		assert!(out.is_empty(), "host slot waits for session audio");
		m.push_session(&session_block(0.5)[..IN_FRAMES]);
		m.pop(&mut out);
		assert!(out.is_empty(), "half a block");
		m.push_session(&session_block(0.5)[IN_FRAMES..]);
		m.pop(&mut out);
		assert_eq!(out.len(), 1);
		let s = &out[0];
		assert_eq!(s.haptics, Some([3; 64]));
		let pcm = s.pcm.as_ref().unwrap();
		assert_eq!(pcm.len(), OUT_FRAMES * 2);
		assert_eq!(pcm[0], 0.25, "fade-in starts at 0 (+ host audio)");
		assert!((pcm[RAMP_FRAMES * 2] - 0.75).abs() < 1e-6, "session + host");
		// No host slot: session alone.
		m.push_session(&session_block(0.9));
		out.clear();
		m.pop(&mut out);
		assert_eq!(out[0].haptics, None);
		assert!((out[0].pcm.as_ref().unwrap()[0] - 0.9).abs() < 1e-6);
	}

	#[test]
	fn host_slots_go_out_alone_when_the_session_is_silent() {
		let mut m = HeadsetMix::default();
		let mut out = Vec::new();
		for i in 0..HOST_LAG as u8 + 2 {
			m.push_host(Some([i; 64]), None);
		}
		m.push_host(None, None);
		m.pop(&mut out);
		assert_eq!(out.len(), 3);
		assert_eq!(out[0].haptics, Some([0; 64]));
		assert_eq!(out[0].pcm, None);
		// The mix stays bounded and a reset fades in again.
		m.reset();
		m.push_session(&session_block(1.0));
		out.clear();
		m.pop(&mut out);
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].pcm.as_ref().unwrap()[0], 0.0);
	}

	#[test]
	fn mixing_clamps() {
		let mut a = vec![0.8, -0.8, 0.1];
		mix_into(&mut a, &[0.5, -0.5, 0.1]);
		assert_eq!(a, vec![1.0, -1.0, 0.2]);
	}

	#[test]
	fn session_backlog_is_bounded() {
		let mut m = HeadsetMix::default();
		for _ in 0..MAX_SESSION + 3 {
			m.push_session(&session_block(0.1));
		}
		assert_eq!(m.dropped, 3);
		let mut out = Vec::new();
		m.pop(&mut out);
		assert_eq!(out.len(), MAX_SESSION);
	}
}
