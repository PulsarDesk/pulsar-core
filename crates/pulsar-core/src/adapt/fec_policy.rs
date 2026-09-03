//! Forward-error-correction sizing (adaptive streaming Phase 2.1 / 2.4): how many media
//! packets one XOR parity packet should cover, given the loss the client reports.
//!
//! Decision taken for the design doc's open question 2: parity overhead is capped at
//! **20 %** of the media rate, parity covers *every* packet (keyframes included — they are
//! the ones whose loss hurts most), and FEC switches off again after a clean stretch.

/// Overhead ceiling (parity bytes / media bytes).
pub const MAX_OVERHEAD: f32 = 0.20;
/// Smallest group (= the 20 % ceiling) and the largest group worth sending.
pub const MIN_N: u8 = 5;
pub const MAX_N: u8 = 32;
/// Loss below which FEC is not worth its overhead.
pub const OFF_BELOW: f32 = 0.005;
/// Clean windows before FEC switches off again.
pub const OFF_AFTER_CLEAN: u32 = 5;

/// Group size for the next parity packets: `0` = FEC off. `loss` is the client's measured
/// ratio for the last window, `prev_n` the size currently in use, `clean_windows` how many
/// consecutive windows sat under `OFF_BELOW`. Parity is sized at roughly **twice** the loss
/// rate (one parity per group repairs one loss, so 2× keeps the repair probability high),
/// clamped to the overhead ceiling, with a ±25 % hysteresis so the size does not chatter.
pub fn group_size(loss: f32, prev_n: u8, clean_windows: u32) -> u8 {
	if loss < OFF_BELOW {
		return if prev_n == 0 || clean_windows >= OFF_AFTER_CLEAN { 0 } else { prev_n };
	}
	let overhead = (loss * 2.0).clamp(1.0 / MAX_N as f32, MAX_OVERHEAD);
	let want = ((1.0 / overhead).round() as u8).clamp(MIN_N, MAX_N);
	if prev_n > 0 {
		let band = (prev_n / 4).max(1);
		if want.abs_diff(prev_n) <= band {
			return prev_n;
		}
	}
	want
}

/// What share of the requested wire rate the *encoder* may use when parity of group size
/// `n` rides along (`n/(n+1)`); 1.0 when FEC is off.
pub fn encoder_share(n: u8) -> f32 {
	if n == 0 {
		1.0
	} else {
		n as f32 / (n as f32 + 1.0)
	}
}

/// Encoder share for a parity *overhead ratio* (parity bytes / media bytes): `1/(1+r)`.
pub fn encoder_share_ratio(overhead: f32) -> f32 {
	1.0 / (1.0 + overhead.max(0.0))
}

// ── Reed-Solomon parity (the gold-standard model: Sunshine/Moonlight, Parsec) ──────────
//
// Per video frame the host appends `m = ceil(k × ratio)` parity packets to the frame's `k`
// packets; the client rebuilds ANY `≤ m` lost packets of that frame with zero round-trips.
// Sunshine ships a fixed 20 % by default; here the ratio follows the measured loss so a
// clean path pays nothing and a bad one gets up to 30 %.

/// Lowest ratio once FEC is on (below this a single parity per frame is all it buys).
pub const RS_MIN_RATIO: f32 = 0.10;
/// Ceiling — the overhead the maintainer accepts on a very lossy path.
pub const RS_MAX_RATIO: f32 = 0.30;
/// Ratio changes smaller than this are ignored (no chatter).
pub const RS_HYSTERESIS: f32 = 0.05;

/// Parity ratio for the next frames: `0.0` = FEC off. Sized at ~2.5× the measured loss
/// (+5 % base) so a frame survives a burst, clamped to `[RS_MIN_RATIO, RS_MAX_RATIO]`; same
/// on/off hysteresis as [`group_size`].
pub fn parity_ratio(loss: f32, prev: f32, clean_windows: u32) -> f32 {
	if loss < OFF_BELOW {
		return if prev <= 0.0 || clean_windows >= OFF_AFTER_CLEAN { 0.0 } else { prev };
	}
	let want = (loss * 2.5 + 0.05).clamp(RS_MIN_RATIO, RS_MAX_RATIO);
	if prev > 0.0 && (want - prev).abs() < RS_HYSTERESIS {
		return prev;
	}
	want
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sizes_follow_loss_within_the_overhead_ceiling() {
		assert_eq!(group_size(0.03, 0, 0), 17, "3 % loss → ~6 % parity");
		assert_eq!(group_size(0.10, 0, 0), 5, "10 % loss hits the 20 % ceiling");
		assert_eq!(group_size(0.30, 0, 0), 5, "never more than 20 % overhead");
		assert_eq!(group_size(0.01, 0, 0), 32, "1 % loss → the largest group");
		assert_eq!(group_size(0.004, 0, 0), 0, "below the floor: off");
	}

	#[test]
	fn hysteresis_and_switch_off_after_a_clean_stretch() {
		assert_eq!(group_size(0.035, 17, 0), 17, "small change keeps the size");
		assert_eq!(group_size(0.08, 17, 0), 6, "big change moves");
		assert_eq!(group_size(0.0, 17, 2), 17, "still on while the clean stretch is short");
		assert_eq!(group_size(0.0, 17, OFF_AFTER_CLEAN), 0, "off after the clean stretch");
	}

	#[test]
	fn rs_parity_ratio_follows_loss_within_bounds_with_hysteresis() {
		assert_eq!(parity_ratio(0.004, 0.0, 0), 0.0, "clean: off");
		assert!((parity_ratio(0.03, 0.0, 0) - 0.125).abs() < 1e-6, "3 % → 12.5 %");
		assert_eq!(parity_ratio(0.10, 0.0, 0), RS_MAX_RATIO, "10 % → ceiling");
		assert_eq!(parity_ratio(0.01, 0.0, 0), RS_MIN_RATIO, "1 % → floor");
		assert!((parity_ratio(0.04, 0.125, 0) - 0.125).abs() < 1e-6, "small change keeps");
		assert!(parity_ratio(0.08, 0.125, 0) > 0.2, "big change moves");
		assert!((parity_ratio(0.0, 0.125, 2) - 0.125).abs() < 1e-6, "short clean stretch keeps");
		assert_eq!(parity_ratio(0.0, 0.125, OFF_AFTER_CLEAN), 0.0, "off after the clean stretch");
		assert!((encoder_share_ratio(0.25) - 0.8).abs() < 1e-6);
		assert_eq!(encoder_share_ratio(0.0), 1.0);
	}

	#[test]
	fn encoder_share_leaves_room_for_parity() {
		assert_eq!(encoder_share(0), 1.0);
		assert!((encoder_share(5) - 0.8333).abs() < 0.001);
		assert!((encoder_share(32) - 0.9697).abs() < 0.001);
	}
}
