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
//!   Windows itself sends sub-120 deltas for precision touchpads, so desktop apps handle
//!   them. GAMES often don't (`delta / WHEEL_DELTA` as an integer turns a 15-unit step into
//!   0, a once-per-event weapon switch steps 8× per notch), so the Windows host injects
//!   [`WheelOut::v_notches`] × 120 instead in a game session or while the client's pointer
//!   is relative (`DesktopInput::set_whole_notch_wheel`); the per-window injector (a
//!   launched game) always does.
//! - **Linux** (uinput): `REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES` (also 120 per notch), plus
//!   the legacy `REL_WHEEL` / `REL_HWHEEL` notch once the hi-res remainder passes HALF a
//!   notch in one direction — what the kernel's hid scroll counters do for a real hi-res
//!   mouse (a wheel resting just short of a detent still fires its notch).
//! - **macOS**: LINE-unit events carrying the exact fractional line count in the fixed-point
//!   delta fields, so every source scrolls on one scale (see [`MacScroll`]).
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
	/// Hi-res units sent that the legacy notches have not accounted for yet (opposite to
	/// `dir` right after a notch fired at the halfway point).
	notch: i32,
	/// Direction of the last hi-res step (+1 / -1, 0 = none yet).
	dir: i8,
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
		let dir: i8 = if hi > 0 { 1 } else { -1 };
		if dir != self.dir {
			self.dir = dir;
			self.notch = 0;
		}
		self.notch = self.notch.saturating_add(hi);
		// Fire the legacy notch at the HALFWAY point, as hid-logitech-hidpp's scroll counter
		// and hid-input's hidinput_handle_scroll do ("some wheels rest 7/8 of a detent from
		// the previous one after slow movement"): waiting for a full 120 delays every notch
		// by half a detent and can drop the last notch of a short scroll.
		if self.notch.abs() < HI_RES_PER_NOTCH / 2 {
			return (hi, 0);
		}
		let mut notches = self.notch / HI_RES_PER_NOTCH;
		if notches == 0 {
			notches = dir as i32;
		}
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
	/// Whole vertical notches completed (legacy `REL_WHEEL`; positive = up). Fires once the
	/// hi-res remainder passes half a notch, like the kernel.
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

/// One macOS scroll event to post (see [`MacScroll`]). Signs are Quartz's: positive = content
/// toward the top (wheel up) / toward the left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacScrollOut {
	/// Whole lines for the integer delta fields (`kCGScrollWheelEventDeltaAxis1/2`) — what the
	/// event is created with; readers of the integer field see whole lines, none lost.
	pub lines_v: i32,
	pub lines_h: i32,
	/// `true` = the delta was whole lines with nothing carried: post the line event exactly
	/// as created (what a notched mouse produces). `false` = also overwrite the fractional
	/// fields below.
	pub plain: bool,
	/// Exact lines of THIS delta (`kCGScrollWheelEventFixedPtDeltaAxis1/2`, behind NSEvent
	/// `deltaY` / `scrollingDeltaY` of a line event).
	pub fixed_v: f64,
	pub fixed_h: f64,
	/// Whole points at [`MacScroll::LINE_PT`] per line, carried
	/// (`kCGScrollWheelEventPointDeltaAxis1/2`).
	pub pt_v: i32,
	pub pt_h: i32,
}

/// macOS scroll: one scale for every source.
///
/// Every delta posts a LINE-unit (non-continuous) wheel event, 100 wire px = 1 line, like a
/// notched mouse. A classic wheel's whole notch is the plain line event it always was; a
/// finer delta (hi-res wheel, precision touchpad, Mac trackpad point deltas, a Windows
/// webview client whose notch isn't exactly 100 px) carries its fractional line count in the
/// fixed-point delta fields, the whole lines in the integer fields (carried) and the points
/// in the point-delta fields (carried, [`Self::LINE_PT`] per line). So the same physical
/// notch moves the same distance whatever wheel or touchpad the client has — an earlier
/// split (whole notches as lines, finer deltas as 1:1 continuous pixels) made hi-res wheels
/// scroll ~10× farther than plain ones. Untested on Mac hardware.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacScroll {
	/// Fractional lines not yet in an integer delta.
	line_v: f64,
	line_h: f64,
	/// Fractional points not yet in a point delta.
	pt_v: f64,
	pt_h: f64,
}

impl MacScroll {
	/// Points per line in the point-delta fields — Quartz's own line → point step for an
	/// unaccelerated wheel event (about 10). Unmeasured on hardware.
	pub const LINE_PT: f64 = 10.0;

	/// Feed one wire delta (browser px, +dx right, +dy down). `None` = nothing to post.
	pub fn push(&mut self, dx: f64, dy: f64) -> Option<MacScrollOut> {
		let (dx, dy) = (if dx.is_finite() { dx } else { 0.0 }, if dy.is_finite() { dy } else { 0.0 });
		if dx == 0.0 && dy == 0.0 {
			return None;
		}
		// Quartz: positive = toward the top (wheel up) / toward the left; browser +dy is down
		// and +dx right, so both negate.
		let (fv, fh) = (-dy / 100.0, -dx / 100.0);
		let c = |x: f64| x.clamp(i32::MIN as f64 / 2.0, i32::MAX as f64 / 2.0) as i32;
		let whole = |l: f64| (l - l.round()).abs() < 1e-6;
		let carried = self.line_v != 0.0 || self.line_h != 0.0 || self.pt_v != 0.0 || self.pt_h != 0.0;
		if !carried && whole(fv) && whole(fh) {
			let (v, h) = (c(fv.round()), c(fh.round()));
			let pt = |l: i32| c(l as f64 * Self::LINE_PT);
			return Some(MacScrollOut {
				lines_v: v,
				lines_h: h,
				plain: true,
				fixed_v: v as f64,
				fixed_h: h as f64,
				pt_v: pt(v),
				pt_h: pt(h),
			});
		}
		// trunc (toward zero) keeps the direction; the 1e-9 nudge keeps float drift
		// (8 × 0.125 summing to 0.9999…) from holding a line back.
		let take = |acc: &mut f64, add: f64| {
			*acc += add;
			let n = (*acc + 1e-9_f64.copysign(*acc)).trunc();
			*acc -= n;
			if acc.abs() < 1e-9 {
				*acc = 0.0;
			}
			c(n)
		};
		let lines_v = take(&mut self.line_v, fv);
		let lines_h = take(&mut self.line_h, fh);
		let pt_v = take(&mut self.pt_v, fv * Self::LINE_PT);
		let pt_h = take(&mut self.pt_h, fh * Self::LINE_PT);
		Some(MacScrollOut { lines_v, lines_h, plain: false, fixed_v: fv, fixed_h: fh, pt_v, pt_h })
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
		// Three notches in one delta: three legacy notches, nothing left over.
		assert_eq!(s.push(0.0, -300.0), WheelOut { v: 360, h: 0, v_notches: 3, h_notches: 0 });
		assert_eq!(s.push(0.0, -50.0).v_notches, 1); // 60 up: the halfway point fires
	}

	#[test]
	fn fine_deltas_fire_the_legacy_notch_halfway() {
		let mut s = HiResScroll::default();
		// 1/8 notch steps (12.5 px = 15 units): the notch fires on the 4th (60 units, half a
		// detent) like the kernel's hid scroll counter, then once per 120.
		let mut fired = vec![];
		let mut units = 0;
		for _ in 0..16 {
			let o = s.push(0.0, -12.5);
			assert_eq!(o.v, 15);
			units += o.v;
			fired.push(o.v_notches);
		}
		assert_eq!(units, 240);
		assert_eq!(fired, [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
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
		assert_eq!(s.push(0.0, -25.0).v_notches, 0); // +30 up, under half a notch
		let o = s.push(0.0, 37.5); // -45 down: the old +30 must not cancel it
		assert_eq!((o.v, o.v_notches), (-45, 0));
		assert_eq!(s.push(0.0, 12.5).v_notches, -1); // -15 more → -60 → one notch down
		// Up again right after a notch fired at the halfway point: the old direction's -60
		// remainder is void too, so this needs its own half notch.
		assert_eq!(s.push(0.0, -25.0).v_notches, 0);
		assert_eq!(s.push(0.0, -25.0).v_notches, 1);
	}

	#[test]
	fn junk_is_ignored() {
		let mut s = HiResScroll::default();
		assert_eq!(s.push(f64::NAN, f64::INFINITY), WheelOut::default());
		assert_eq!(s.push(0.0, 100.0).v, -120);
	}

	#[test]
	fn mac_whole_notches_are_plain_line_events() {
		let mut m = MacScroll::default();
		let o = m.push(0.0, 100.0).unwrap();
		assert_eq!((o.lines_v, o.lines_h, o.plain), (-1, 0, true));
		let o = m.push(0.0, -300.0).unwrap();
		assert_eq!((o.lines_v, o.plain), (3, true));
		let o = m.push(100.0, 0.0).unwrap();
		assert_eq!((o.lines_v, o.lines_h, o.plain), (0, -1, true));
		assert_eq!(m.push(0.0, 0.0), None);
	}

	#[test]
	fn mac_a_hi_res_notch_moves_as_far_as_a_plain_one() {
		let mut m = MacScroll::default();
		let (mut lines, mut fixed, mut pt) = (0, 0.0, 0);
		for _ in 0..8 {
			let o = m.push(0.0, 12.5).unwrap();
			assert!(!o.plain);
			lines += o.lines_v;
			fixed += o.fixed_v;
			pt += o.pt_v;
		}
		// The same distance as one plain notch (1 line, LINE_PT points), on every field.
		let plain = MacScroll::default().push(0.0, 100.0).unwrap();
		assert_eq!(lines, plain.lines_v);
		assert!((fixed - plain.fixed_v).abs() < 1e-9);
		assert_eq!(pt, plain.pt_v);
		// Nothing carried any more: the next whole notch is plain again.
		assert!(m.push(0.0, 100.0).unwrap().plain);
	}

	#[test]
	fn mac_an_odd_notch_size_scales_linearly() {
		// A Windows webview client with a non-default lines-per-notch: 166.67 px per notch.
		let mut m = MacScroll::default();
		let o = m.push(0.0, 500.0 / 3.0).unwrap();
		assert!(!o.plain);
		assert_eq!(o.lines_v, -1);
		assert!((o.fixed_v + 5.0 / 3.0).abs() < 1e-9);
		assert_eq!(o.pt_v, -16);
		let o = m.push(0.0, 500.0 / 3.0).unwrap();
		assert_eq!((o.lines_v, o.pt_v), (-2, -17)); // 3.33 lines / 33.3 pt so far, carried
	}
}
