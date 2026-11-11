//! Client-side DualSense settings and the automatic feature shedding policy
//! ("otomatik kısma").
//!
//! A Bluetooth DualSense shares a narrow radio link (often with 2.4 GHz Wi-Fi on the same
//! combo chip). When the link can't keep up, the client turns off the heaviest features
//! first: pad speaker, then pad mic, then HD haptics (the pad falls back to classic rumble
//! made from the haptics energy, see [`super::rumble`]). It turns them back on one by one
//! after the link has been quiet for a while.
//!
//! Everything here is pure: the pad thread feeds [`LinkMonitor`] with input-report arrival
//! times and [`super::pad::PadStats`], gets a [`Window`] every [`WINDOW`], and hands it to
//! [`Degrader::on_window`], which says what to shed or restore.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::pad::PadStats;
use super::Ds5Features;

/// Per-feature user choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
	/// On, but auto-degrade may turn it off while the link is weak.
	#[default]
	Auto,
	/// Always on, never shed.
	On,
	/// Off.
	Off,
}

impl Mode {
	pub fn enabled(self) -> bool {
		self != Mode::Off
	}
}

/// The client's DualSense settings (Settings → Kontroller → DualSense). JSON field names
/// match the UI store (`ui.ds5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Ds5Prefs {
	/// Master switch: off = the pad is forwarded as a plain gamepad (`GamepadSlot`).
	pub enabled: bool,
	pub triggers: Mode,
	pub haptics: Mode,
	pub speaker: Mode,
	pub mic: Mode,
	pub leds: Mode,
	pub rumble: Mode,
	/// Shed `Auto` features while the Bluetooth link is weak.
	pub auto_degrade: bool,
	/// Haptics/speaker jitter buffer on the client, in milliseconds.
	pub jitter_ms: u32,
}

/// Default haptics jitter buffer (6 slots of 10.667 ms).
pub const DEFAULT_JITTER_MS: u32 = 64;

impl Default for Ds5Prefs {
	fn default() -> Self {
		Self {
			enabled: true,
			triggers: Mode::Auto,
			haptics: Mode::Auto,
			speaker: Mode::Auto,
			mic: Mode::Auto,
			leds: Mode::Auto,
			rumble: Mode::Auto,
			auto_degrade: true,
			jitter_ms: DEFAULT_JITTER_MS,
		}
	}
}

impl Ds5Prefs {
	/// Features the user allows (`Off` = off; `Auto` and `On` = on), before shedding.
	pub fn features(&self) -> Ds5Features {
		Ds5Features {
			triggers: self.triggers.enabled(),
			rumble: self.rumble.enabled(),
			leds: self.leds.enabled(),
			haptics: self.haptics.enabled(),
			speaker: self.speaker.enabled(),
			mic: self.mic.enabled(),
		}
	}

	fn mode(&self, f: Shed) -> Mode {
		match f {
			Shed::Speaker => self.speaker,
			Shed::Mic => self.mic,
			Shed::Haptics => self.haptics,
		}
	}

	/// Jitter buffer as `Pad::set_jitter_buffer` slots: `(prebuffer, max)`.
	pub fn jitter_slots(&self) -> (usize, usize) {
		let ms = self.jitter_ms.clamp(11, 500) as f64;
		let pre = (ms / 10.667).round().max(1.0) as usize;
		(pre, pre * 4)
	}
}

/// A feature auto-degrade can turn off, in shedding order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shed {
	Speaker,
	Mic,
	Haptics,
}

/// Shedding order: the most expensive for the least loss first.
pub const SHED_ORDER: [Shed; 3] = [Shed::Speaker, Shed::Mic, Shed::Haptics];

impl Shed {
	fn clear(self, f: &mut Ds5Features) {
		match self {
			Shed::Speaker => f.speaker = false,
			Shed::Mic => f.mic = false,
			Shed::Haptics => f.haptics = false,
		}
	}

	fn get(self, f: &Ds5Features) -> bool {
		match self {
			Shed::Speaker => f.speaker,
			Shed::Mic => f.mic,
			Shed::Haptics => f.haptics,
		}
	}
}

/// One shed or restore step, for the user notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
	pub feature: Shed,
	/// True = turned off; false = turned back on.
	pub shed: bool,
}

/// Length of one measurement window.
pub const WINDOW: Duration = Duration::from_secs(2);
/// Consecutive bad windows before shedding one feature.
pub const BAD_WINDOWS: u32 = 2;
/// Consecutive good windows before restoring one feature (30 s).
pub const GOOD_WINDOWS: u32 = 15;
/// The recovery wait doubles each time a restored feature has to be shed again soon,
/// up to this factor (hysteresis against flapping on a borderline link).
pub const MAX_BACKOFF: u32 = 8;
/// A shed within this many windows of a restore counts as "again soon".
const FLAP_WINDOWS: u32 = 30;

/// Thresholds for a bad window. The real session measured max BT writes of ~13 ms with
/// the mic streaming. Input nominally arrives every 4 ms, but a healthy Windows BT link
/// with audio streaming runs at ~140 Hz with single gaps of 60-75 ms (2026-11-11 session:
/// a 60 ms limit shed speaker and haptics within 10 s on a good link), so only a gap
/// long enough to be felt counts.
pub const BAD_WRITE_US: u64 = 25_000;
pub const BAD_INPUT_GAP: Duration = Duration::from_millis(150);
/// Windows ignored at the start of a pad's session (connection setup stalls the link).
pub const WARMUP_WINDOWS: u32 = 3;
pub const BAD_OVERFLOWS: u64 = 3;
/// Underruns mostly mean the network stream was late, not the pad link; only a lot of
/// them counts.
pub const BAD_UNDERRUNS: u64 = 8;

/// What happened on the pad link during one window (deltas, except the maxima).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
	pub write_errors: u64,
	pub underruns: u64,
	pub overflows: u64,
	/// Longest write in this window.
	pub max_write_us: u64,
	/// Input reports received.
	pub inputs: u32,
	/// Longest gap between two input reports (or since the window start).
	pub input_gap_max: Duration,
}

impl Window {
	pub fn is_bad(&self) -> bool {
		self.write_errors > 0
			|| self.overflows >= BAD_OVERFLOWS
			|| self.underruns >= BAD_UNDERRUNS
			|| self.max_write_us >= BAD_WRITE_US
			|| self.input_gap_max >= BAD_INPUT_GAP
	}
}

/// Turns pad events into [`Window`]s.
#[derive(Debug)]
pub struct LinkMonitor {
	start: Instant,
	base: PadStats,
	last_input: Instant,
	gap_max: Duration,
	inputs: u32,
	write_max: u64,
}

impl LinkMonitor {
	pub fn new(now: Instant, stats: PadStats) -> Self {
		Self {
			start: now,
			base: stats,
			last_input: now,
			gap_max: Duration::ZERO,
			inputs: 0,
			write_max: 0,
		}
	}

	/// An input report arrived.
	pub fn on_input(&mut self, now: Instant) {
		self.gap_max = self
			.gap_max
			.max(now.saturating_duration_since(self.last_input));
		self.last_input = now;
		self.inputs += 1;
	}

	/// Call often; returns the finished window once [`WINDOW`] has passed. `stats` = the
	/// pad's counters now, `max_write_us` = the longest write since the previous call
	/// (`Pad::take_window_max_write_us`).
	pub fn tick(&mut self, now: Instant, stats: PadStats, max_write_us: u64) -> Option<Window> {
		self.write_max = self.write_max.max(max_write_us);
		if now.saturating_duration_since(self.start) < WINDOW {
			return None;
		}
		let w = Window {
			write_errors: stats.write_errors.saturating_sub(self.base.write_errors),
			underruns: stats.underruns.saturating_sub(self.base.underruns),
			overflows: stats.overflows.saturating_sub(self.base.overflows),
			max_write_us: std::mem::take(&mut self.write_max),
			inputs: self.inputs,
			input_gap_max: self
				.gap_max
				.max(now.saturating_duration_since(self.last_input)),
		};
		self.start = now;
		self.base = stats;
		self.gap_max = Duration::ZERO;
		self.inputs = 0;
		Some(w)
	}
}

/// The shedding state machine for one pad.
#[derive(Debug, Default)]
pub struct Degrader {
	/// Shed features, in the order they were shed.
	shed: Vec<Shed>,
	bad: u32,
	good: u32,
	/// Current recovery wait factor (1..=MAX_BACKOFF).
	backoff: u32,
	/// Windows since the last restore (for flap detection).
	since_restore: Option<u32>,
}

impl Degrader {
	pub fn new() -> Self {
		Self {
			backoff: 1,
			..Default::default()
		}
	}

	/// Features currently shed.
	pub fn shed(&self) -> &[Shed] {
		&self.shed
	}

	/// The features to apply: the user's choice minus what is shed.
	pub fn features(&self, prefs: &Ds5Prefs) -> Ds5Features {
		let mut f = prefs.features();
		for s in &self.shed {
			s.clear(&mut f);
		}
		f
	}

	/// The settings changed: forget sheds that no longer apply (the feature is no longer
	/// `Auto`, or auto-degrade was turned off). Silent: the user made the change.
	pub fn reconcile(&mut self, prefs: &Ds5Prefs) {
		if !prefs.auto_degrade {
			self.shed.clear();
		} else {
			self.shed.retain(|s| prefs.mode(*s) == Mode::Auto);
		}
		self.bad = 0;
		self.good = 0;
	}

	/// Judge one window. `live` = features actually in use on this pad (a feature that
	/// isn't flowing, like a closed mic, is skipped: shedding it would gain nothing).
	pub fn on_window(&mut self, w: &Window, prefs: &Ds5Prefs, live: Ds5Features) -> Option<Change> {
		if let Some(n) = self.since_restore.as_mut() {
			*n += 1;
		}
		if !prefs.auto_degrade {
			self.bad = 0;
			self.good = 0;
			return None;
		}
		if w.is_bad() {
			self.good = 0;
			self.bad += 1;
			if self.bad < BAD_WINDOWS {
				return None;
			}
			self.bad = 0;
			let next = SHED_ORDER
				.into_iter()
				.find(|s| prefs.mode(*s) == Mode::Auto && s.get(&live) && !self.shed.contains(s))?;
			if self.since_restore.is_some_and(|n| n <= FLAP_WINDOWS) {
				self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
			}
			self.since_restore = None;
			self.shed.push(next);
			return Some(Change {
				feature: next,
				shed: true,
			});
		}
		self.bad = 0;
		if self.shed.is_empty() {
			self.good = 0;
			self.settle();
			return None;
		}
		self.good += 1;
		if self.good < GOOD_WINDOWS * self.backoff {
			return None;
		}
		self.good = 0;
		let f = self.shed.pop()?;
		self.since_restore = Some(0);
		Some(Change {
			feature: f,
			shed: false,
		})
	}

	/// The link stayed healthy long after the last restore: forget the backoff.
	fn settle(&mut self) {
		if self.since_restore.is_some_and(|n| n > FLAP_WINDOWS) && self.shed.is_empty() {
			self.backoff = 1;
			self.since_restore = None;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bad() -> Window {
		Window {
			write_errors: 1,
			..Default::default()
		}
	}
	fn good() -> Window {
		Window::default()
	}
	fn all_live() -> Ds5Features {
		Ds5Features::all()
	}

	#[test]
	fn prefs_default_and_json() {
		let p = Ds5Prefs::default();
		assert!(p.enabled && p.auto_degrade);
		assert_eq!(p.features(), Ds5Features::all());
		let j: Ds5Prefs = serde_json::from_str(
			r#"{"enabled":false,"speaker":"off","haptics":"on","autoDegrade":false}"#,
		)
		.unwrap();
		assert!(!j.enabled && !j.auto_degrade);
		assert_eq!(j.speaker, Mode::Off);
		assert_eq!(j.haptics, Mode::On);
		assert_eq!(j.mic, Mode::Auto, "missing fields default");
		assert_eq!(j.jitter_ms, DEFAULT_JITTER_MS);
		assert!(!j.features().speaker);
		assert_eq!(Ds5Prefs::default().jitter_slots(), (6, 24));
	}

	#[test]
	fn window_thresholds() {
		assert!(!good().is_bad());
		assert!(bad().is_bad());
		let w = |f: fn(&mut Window)| {
			let mut x = Window::default();
			f(&mut x);
			x.is_bad()
		};
		assert!(!w(|x| x.overflows = BAD_OVERFLOWS - 1));
		assert!(w(|x| x.overflows = BAD_OVERFLOWS));
		assert!(!w(|x| x.underruns = 2));
		assert!(w(|x| x.max_write_us = 30_000));
		assert!(!w(|x| x.max_write_us = 13_000));
		// A healthy Windows BT link with audio shows single 60-75 ms gaps.
		assert!(!w(|x| x.input_gap_max = Duration::from_millis(75)));
		assert!(w(|x| x.input_gap_max = Duration::from_millis(200)));
	}

	#[test]
	fn sheds_in_order_after_two_bad_windows() {
		let p = Ds5Prefs::default();
		let mut d = Degrader::new();
		assert_eq!(d.on_window(&bad(), &p, all_live()), None);
		let c = d.on_window(&bad(), &p, all_live()).unwrap();
		assert_eq!(
			c,
			Change {
				feature: Shed::Speaker,
				shed: true
			}
		);
		assert!(!d.features(&p).speaker);
		assert_eq!(d.on_window(&bad(), &p, all_live()), None);
		assert_eq!(
			d.on_window(&bad(), &p, all_live()).unwrap().feature,
			Shed::Mic
		);
		d.on_window(&bad(), &p, all_live());
		assert_eq!(
			d.on_window(&bad(), &p, all_live()).unwrap().feature,
			Shed::Haptics
		);
		let f = d.features(&p);
		assert!(!f.haptics && !f.mic && !f.speaker);
		assert!(f.rumble && f.triggers && f.leds, "classic features stay");
		// Nothing left to shed.
		d.on_window(&bad(), &p, all_live());
		assert_eq!(d.on_window(&bad(), &p, all_live()), None);
	}

	#[test]
	fn a_good_window_resets_the_bad_streak() {
		let p = Ds5Prefs::default();
		let mut d = Degrader::new();
		for _ in 0..5 {
			assert_eq!(d.on_window(&bad(), &p, all_live()), None);
			assert_eq!(d.on_window(&good(), &p, all_live()), None);
		}
		assert!(d.shed().is_empty());
	}

	#[test]
	fn respects_overrides_and_live_features() {
		let p = Ds5Prefs {
			speaker: Mode::On,
			..Default::default()
		};
		let live = Ds5Features {
			mic: false,
			..Ds5Features::all()
		};
		let mut d = Degrader::new();
		d.on_window(&bad(), &p, live);
		// Speaker is "Açık" and the mic is not in use: haptics is next.
		assert_eq!(
			d.on_window(&bad(), &p, live).unwrap().feature,
			Shed::Haptics
		);

		let off = Ds5Prefs {
			speaker: Mode::Off,
			mic: Mode::Off,
			haptics: Mode::On,
			..Default::default()
		};
		let mut d = Degrader::new();
		for _ in 0..6 {
			assert_eq!(d.on_window(&bad(), &off, all_live()), None);
		}
	}

	#[test]
	fn auto_degrade_off_never_sheds_and_clears() {
		let mut p = Ds5Prefs::default();
		let mut d = Degrader::new();
		d.on_window(&bad(), &p, all_live());
		d.on_window(&bad(), &p, all_live());
		assert_eq!(d.shed(), &[Shed::Speaker]);
		p.auto_degrade = false;
		d.reconcile(&p);
		assert!(d.shed().is_empty());
		for _ in 0..6 {
			assert_eq!(d.on_window(&bad(), &p, all_live()), None);
		}
	}

	#[test]
	fn reconcile_drops_sheds_that_are_no_longer_auto() {
		let mut p = Ds5Prefs::default();
		let mut d = Degrader::new();
		for _ in 0..4 {
			d.on_window(&bad(), &p, all_live());
		}
		assert_eq!(d.shed(), &[Shed::Speaker, Shed::Mic]);
		p.speaker = Mode::On;
		d.reconcile(&p);
		assert_eq!(d.shed(), &[Shed::Mic]);
		assert!(d.features(&p).speaker);
	}

	#[test]
	fn recovers_one_step_after_30s_with_backoff() {
		let p = Ds5Prefs::default();
		let mut d = Degrader::new();
		for _ in 0..4 {
			d.on_window(&bad(), &p, all_live());
		}
		assert_eq!(d.shed(), &[Shed::Speaker, Shed::Mic]);
		for _ in 0..GOOD_WINDOWS - 1 {
			assert_eq!(d.on_window(&good(), &p, all_live()), None);
		}
		// Last shed comes back first.
		assert_eq!(
			d.on_window(&good(), &p, all_live()),
			Some(Change {
				feature: Shed::Mic,
				shed: false
			})
		);
		// Trouble again right after the restore: shed, and the next recovery waits twice
		// as long.
		d.on_window(&bad(), &p, all_live());
		assert_eq!(
			d.on_window(&bad(), &p, all_live()).unwrap().feature,
			Shed::Mic
		);
		for _ in 0..GOOD_WINDOWS * 2 - 1 {
			assert_eq!(d.on_window(&good(), &p, all_live()), None);
		}
		assert_eq!(
			d.on_window(&good(), &p, all_live()).unwrap().feature,
			Shed::Mic
		);
		for _ in 0..GOOD_WINDOWS * 2 - 1 {
			assert_eq!(d.on_window(&good(), &p, all_live()), None);
		}
		assert_eq!(
			d.on_window(&good(), &p, all_live()).unwrap().feature,
			Shed::Speaker
		);
		assert!(d.shed().is_empty());
		for _ in 0..FLAP_WINDOWS + 1 {
			d.on_window(&good(), &p, all_live());
		}
		assert_eq!(d.backoff, 1);
	}

	#[test]
	fn monitor_builds_windows() {
		let t0 = Instant::now();
		let mut m = LinkMonitor::new(t0, PadStats::default());
		m.on_input(t0 + Duration::from_millis(4));
		m.on_input(t0 + Duration::from_millis(100));
		assert_eq!(
			m.tick(t0 + Duration::from_secs(1), PadStats::default(), 9000),
			None
		);
		let stats = PadStats {
			write_errors: 2,
			overflows: 1,
			..Default::default()
		};
		let w = m.tick(t0 + WINDOW, stats, 500).unwrap();
		assert_eq!(w.inputs, 2);
		assert_eq!(w.write_errors, 2);
		assert_eq!(w.overflows, 1);
		assert_eq!(w.max_write_us, 9000);
		// No input since 100 ms: the open gap to the window end counts.
		assert!(w.input_gap_max >= Duration::from_millis(1900));
		// Next window is a delta.
		m.on_input(t0 + WINDOW + Duration::from_millis(4));
		let w2 = m.tick(t0 + WINDOW * 2, stats, 0).unwrap();
		assert_eq!(w2.write_errors, 0);
		assert_eq!(w2.max_write_us, 0);
		assert_eq!(w2.inputs, 1);
	}
}
