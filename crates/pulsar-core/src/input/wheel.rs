//! High-resolution scroll: the wire unit → what each host OS injects.
//!
//! The wire ([`InputEvent::Scroll`](crate::service::InputEvent::Scroll)) carries BROWSER
//! wheel pixels as `f64`: 100 px = one wheel notch, +y = down, +x = right. Fractions are
//! meaningful — a precision touchpad, a free-spinning / hi-res wheel or a Mac trackpad sends
//! many small deltas, and a host must not round them to whole notches (that turned smooth
//! scrolling into 100 px steps and swallowed slow scrolls until a notch had built up).
//!
//! Every host injects in the OS's own fine unit instead:
//! - **Windows**: `mouseData` in `WHEEL_DELTA` (120) fractions — 1/120 notch per unit;
//!   Windows itself sends sub-120 deltas for precision touchpads, so apps handle them.
//! - **Linux** (uinput): `REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES` (also 120 per notch), plus
//!   the legacy `REL_WHEEL` / `REL_HWHEEL` whole notch every time 120 hi-res units built up
//!   in one direction — exactly what the kernel's own hid driver does for a hi-res mouse.
//! - **macOS**: pixel-unit continuous scroll for smooth input, line units for plain notches
//!   (see [`MacScroll`]).
//!
//! Pure + platform-independent so the conversions are unit-tested on every target.

/// Hi-res units (1/120 notch) per browser wheel pixel: 120 per notch / 100 px per notch.
pub const HI_RES_PER_PX: f64 = 1.2;
/// Hi-res units per wheel notch (`WHEEL_DELTA`, the kernel's hi-res scale).
pub const HI_RES_PER_NOTCH: i32 = 120;

/// One axis of [`HiResScroll`].
#[derive(Clone, Copy, Debug, Default)]
struct Axis {
	/// Sub-unit carry (fractions of 1/120 notch not sent yet).
	frac: f64,
	/// Hi-res units sent in the current direction that have not completed a whole notch.
	notch: i32,
}

impl Axis {
	/// `units` = signed hi-res units (fractional). Returns (whole hi-res units to send now,
	/// whole legacy notches completed by them).
	fn push(&mut self, units: f64) -> (i32, i32) {
		if !units.is_finite() {
			return (0, 0);
		}
		let total = self.frac + units;
		// trunc (toward zero) keeps the direction; the remainder carries to the next call.
		// The 1e-9 nudge keeps float drift (10 × 0.6 = 5.999…) from holding a unit back.
		let nudged = total + 1e-9_f64.copysign(total);
		let hi = nudged.trunc().clamp(i32::MIN as f64 / 2.0, i32::MAX as f64 / 2.0) as i32;
		self.frac = total - hi as f64;
		if hi == 0 {
			return (0, 0);
		}
		// Direction change: the half-built notch of the old direction is void (the kernel's
		// hid scroll counter resets the same way), so a reversal doesn't fire a stale notch.
		if self.notch != 0 && (self.notch > 0) != (hi > 0) {
			self.notch = 0;
		}
		self.notch = self.notch.saturating_add(hi);
		let notches = self.notch / HI_RES_PER_NOTCH;
		self.notch -= notches * HI_RES_PER_NOTCH;
		(hi, notches)
	}
}

/// Browser-pixel scroll → hi-res wheel units (1/120 notch) with sub-unit carry, in the
/// wheel's own sign convention: `v` positive = wheel UP / away from the user (Windows
/// `MOUSEEVENTF_WHEEL`, evdev `REL_WHEEL`), `h` positive = RIGHT. The browser's +dy is
/// down, so the vertical axis is negated here — callers pass the wire values unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct HiResScroll {
	v: Axis,
	h: Axis,
}

/// What [`HiResScroll::push`] says to inject now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WheelOut {
	/// Vertical hi-res units (positive = up).
	pub v: i32,
	/// Horizontal hi-res units (positive = right).
	pub h: i32,
	/// Whole vertical notches completed (legacy `REL_WHEEL`; positive = up).
	pub v_notches: i32,
	/// Whole horizontal notches completed (legacy `REL_HWHEEL`; positive = right).
	pub h_notches: i32,
}

impl HiResScroll {
	/// Feed one wire scroll delta (browser px, +dx right, +dy down).
	pub fn push(&mut self, dx: f64, dy: f64) -> WheelOut {
		let (v, v_notches) = self.v.push(-dy * HI_RES_PER_PX);
		let (h, h_notches) = self.h.push(dx * HI_RES_PER_PX);
		WheelOut { v, h, v_notches, h_notches }
	}
}

/// How a macOS host posts one scroll delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacScrollOut {
	/// Nothing to post (sub-pixel so far).
	None,
	/// `kCGScrollEventUnitLine` whole lines (`v` positive = content toward the top / wheel
	/// up, `h` positive = toward the left), like a plain notched mouse — keeps macOS's own
	/// line acceleration for ordinary wheels.
	Line { v: i32, h: i32 },
	/// `kCGScrollEventUnitPixel` (continuous) whole pixels, same signs as `Line`.
	Pixel { v: i32, h: i32 },
}

/// macOS scroll unit choice + pixel carry.
///
/// A delta that is a whole number of notches (a classic wheel, or a Mac client's line
/// wheel) posts LINE units as before: one notch = one line, which macOS accelerates like a
/// local mouse. Anything finer (precision touchpad, hi-res wheel, Mac trackpad point
/// deltas) posts continuous PIXEL units 1:1 with the wire's browser pixels; once a burst
/// went smooth it STAYS smooth until the scroll has been idle [`MacScroll::SMOOTH_HOLD_MS`],
/// so a hi-res wheel that happens to land on a whole notch doesn't flip units mid-gesture.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacScroll {
	frac_v: f64,
	frac_h: f64,
	/// When the last smooth (pixel) delta was posted, in caller milliseconds.
	smooth_at: Option<u64>,
}

impl MacScroll {
	/// A smooth burst keeps pixel units until this long without a scroll event.
	pub const SMOOTH_HOLD_MS: u64 = 250;

	/// [`push`](Self::push) on the process's monotonic clock.
	pub fn push_now(&mut self, dx: f64, dy: f64) -> MacScrollOut {
		static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
		let now = T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
		self.push(now, dx, dy)
	}

	/// Feed one wire delta (browser px, +dx right, +dy down) at `now_ms` (any monotonic
	/// millisecond clock).
	pub fn push(&mut self, now_ms: u64, dx: f64, dy: f64) -> MacScrollOut {
		let (dx, dy) = (if dx.is_finite() { dx } else { 0.0 }, if dy.is_finite() { dy } else { 0.0 });
		if dx == 0.0 && dy == 0.0 {
			return MacScrollOut::None;
		}
		let whole = |p: f64| (p / 100.0 - (p / 100.0).round()).abs() < 1e-6;
		let smooth_burst = self.smooth_at.is_some_and(|t| now_ms.saturating_sub(t) < Self::SMOOTH_HOLD_MS);
		if !smooth_burst {
			// A finished smooth burst's sub-pixel leftover is not worth a stale pixel later.
			self.frac_v = 0.0;
			self.frac_h = 0.0;
		}
		if !smooth_burst && whole(dx) && whole(dy) {
			self.smooth_at = None;
			// Quartz: positive = toward the top (wheel up) / toward the left; browser +dy is
			// down and +dx right, so both negate.
			return MacScrollOut::Line { v: -(dy / 100.0).round() as i32, h: -(dx / 100.0).round() as i32 };
		}
		self.smooth_at = Some(now_ms);
		self.frac_v += -dy;
		self.frac_h += -dx;
		let v = self.frac_v.trunc();
		let h = self.frac_h.trunc();
		self.frac_v -= v;
		self.frac_h -= h;
		if v == 0.0 && h == 0.0 {
			return MacScrollOut::None;
		}
		let c = |x: f64| x.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
		MacScrollOut::Pixel { v: c(v), h: c(h) }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_notch_is_120_units_and_one_legacy_notch() {
		let mut s = HiResScroll::default();
		// browser +100 px = one notch DOWN → wheel -120.
		assert_eq!(s.push(0.0, 100.0), WheelOut { v: -120, h: 0, v_notches: -1, h_notches: 0 });
		assert_eq!(s.push(-100.0, 0.0), WheelOut { v: 0, h: -120, v_notches: 0, h_notches: -1 });
	}

	#[test]
	fn fine_deltas_send_hi_res_and_complete_notches_later() {
		let mut s = HiResScroll::default();
		// 1/8 notch steps (12.5 px = 15 units): eight of them complete one legacy notch.
		let mut notches = 0;
		let mut units = 0;
		for _ in 0..8 {
			let o = s.push(0.0, -12.5);
			assert_eq!(o.v, 15);
			units += o.v;
			notches += o.v_notches;
		}
		assert_eq!((units, notches), (120, 1));
	}

	#[test]
	fn sub_unit_deltas_carry() {
		let mut s = HiResScroll::default();
		// 0.5 px = 0.6 units: nothing, then 1 unit (1.2), then nothing (0.8), then 1 (1.4)...
		let sent: i32 = (0..10).map(|_| s.push(0.0, -0.5).v).sum();
		assert_eq!(sent, 6); // 10 × 0.6 = 6 units, none lost
	}

	#[test]
	fn reversal_voids_the_half_built_notch() {
		let mut s = HiResScroll::default();
		assert_eq!(s.push(0.0, -50.0).v_notches, 0); // +60 up
		let o = s.push(0.0, 75.0); // -90 down: the old +60 must not cancel it into 0
		assert_eq!((o.v, o.v_notches), (-90, 0));
		assert_eq!(s.push(0.0, 25.0).v_notches, -1); // -30 more → -120 → one notch down
	}

	#[test]
	fn junk_is_ignored() {
		let mut s = HiResScroll::default();
		assert_eq!(s.push(f64::NAN, f64::INFINITY), WheelOut::default());
		assert_eq!(s.push(0.0, 100.0).v, -120);
	}

	#[test]
	fn mac_notches_stay_lines() {
		let mut m = MacScroll::default();
		assert_eq!(m.push(0, 0.0, 100.0), MacScrollOut::Line { v: -1, h: 0 });
		assert_eq!(m.push(10, 0.0, -300.0), MacScrollOut::Line { v: 3, h: 0 });
		assert_eq!(m.push(20, 100.0, 0.0), MacScrollOut::Line { v: 0, h: -1 });
	}

	#[test]
	fn mac_fine_deltas_are_pixels_and_the_burst_stays_smooth() {
		let mut m = MacScroll::default();
		assert_eq!(m.push(0, 0.0, 12.5), MacScrollOut::Pixel { v: -12, h: 0 });
		// A whole notch inside the smooth burst: still pixels (carry 0.5 + 100).
		assert_eq!(m.push(10, 0.0, 100.0), MacScrollOut::Pixel { v: -100, h: 0 });
		assert_eq!(m.push(20, 0.0, 0.5), MacScrollOut::Pixel { v: -1, h: 0 });
		// Idle past the hold: a whole notch is a line again.
		assert_eq!(m.push(20 + MacScroll::SMOOTH_HOLD_MS, 0.0, 100.0), MacScrollOut::Line { v: -1, h: 0 });
	}

	#[test]
	fn mac_sub_pixel_carries() {
		let mut m = MacScroll::default();
		assert_eq!(m.push(0, 0.0, 0.4), MacScrollOut::None);
		assert_eq!(m.push(1, 0.0, 0.4), MacScrollOut::None);
		assert_eq!(m.push(2, 0.0, 0.4), MacScrollOut::Pixel { v: -1, h: 0 });
	}
}
