//! The adaptive controller's ceiling: what a clean picture needs when nobody set a limit
//! (`auto_cap_kbps`), and the cap of the path the media runs on now (`path_cap_kbps`). Shared by
//! the desktop (play.rs / hold.rs) and the mobile client.

/// Upper bound of the mobile client's automatic ceiling (kbit/s): a phone / tablet decoder and
/// its Wi-Fi radio. The old fixed 12 Mbit cap held a 1440p+ tablet on a LAN at 12 Mbit and also
/// cut the start probe's plan short; 50 Mbit is past what a phone shows a difference for.
pub const MOBILE_AUTO_CAP_MAX_KBPS: u32 = 50_000;

/// The adaptive controller's ceiling when neither the user nor the transport set one: what a
/// clean picture of the native point needs, with headroom. Moonlight-like reference (≈ 20 Mbit
/// for 1080p60 H.264 game content), scaled by pixels, by fps^0.75 (more frames share more
/// redundancy), and by the codec's efficiency; desktop content needs ~60 % of that. Clamped
/// to 15..150 Mbit. The controller still only climbs as far as the path stays clean.
pub fn auto_cap_kbps(codec: &str, native: (u32, u32, u32), game_mode: bool) -> u32 {
	let (w, h, fps) = native;
	let (w, h) = if w == 0 || h == 0 { (1920, 1080) } else { (w, h) };
	let fps = if fps == 0 { 60 } else { fps.max(30) };
	let px = (w as f64 * h as f64) / (1920.0 * 1080.0);
	let f = (fps as f64 / 60.0).powf(0.75);
	let eff = match codec.to_ascii_lowercase().as_str() {
		"h265" | "hevc" => 0.75,
		"av1" => 0.65,
		_ => 1.0,
	};
	let mode = if game_mode { 1.0 } else { 0.6 };
	((20_000.0 * px * f * eff * mode) as u32).clamp(15_000, 150_000)
}

/// `auto_cap_kbps` for the mobile client: at most `MOBILE_AUTO_CAP_MAX_KBPS`.
pub fn mobile_auto_cap_kbps(codec: &str, native: (u32, u32, u32), game_mode: bool) -> u32 {
	auto_cap_kbps(codec, native, game_mode).min(MOBILE_AUTO_CAP_MAX_KBPS)
}

/// The relay's per-session forwarding cap as the controller's cap (kbit/s): 10 % headroom for
/// audio + RTP/IP overhead, never under 2 Mbit. 0 = uncapped.
pub fn relay_cap_kbps(relay_cap_raw: u32) -> u32 {
	if relay_cap_raw > 0 {
		(relay_cap_raw * 9 / 10).max(2_000)
	} else {
		0
	}
}

/// The adaptive cap for the path the media runs on now: on the relay its per-session
/// forwarding cap (`relay_cap_raw`, 0 = uncapped; see `relay_cap_kbps`) bounds the direct-path
/// cap; direct = `direct_cap` alone.
pub fn path_cap_kbps(relay: bool, relay_cap_raw: u32, direct_cap: u32) -> u32 {
	if relay && relay_cap_raw > 0 {
		let c = relay_cap_kbps(relay_cap_raw);
		if direct_cap > 0 {
			c.min(direct_cap)
		} else {
			c
		}
	} else {
		direct_cap
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn auto_cap_scales_with_pixels_fps_codec_and_mode() {
		let game_1200p120_hevc = auto_cap_kbps("h265", (1920, 1200, 120), true);
		assert!((26_000..30_000).contains(&game_1200p120_hevc), "{game_1200p120_hevc}");
		let remote = auto_cap_kbps("h265", (1920, 1200, 120), false);
		assert!(remote < game_1200p120_hevc && remote >= 15_000, "{remote}");
		let uhd = auto_cap_kbps("h265", (3840, 2160, 60), true);
		assert!((55_000..65_000).contains(&uhd), "{uhd}");
		assert_eq!(auto_cap_kbps("h264", (1280, 720, 30), false), 15_000, "never under 15 Mbit");
		assert_eq!(auto_cap_kbps("h264", (0, 0, 0), true), 20_000, "unknown → 1080p60 reference");
		assert_eq!(auto_cap_kbps("h264", (7680, 4320, 120), true), 150_000, "never over 150 Mbit");
	}

	#[test]
	fn the_mobile_cap_is_bounded_but_lifts_the_old_12_mbit() {
		// A 1440p60 tablet on a LAN: past the old 12 Mbit, under the mobile bound.
		let tab = mobile_auto_cap_kbps("h265", (2560, 1600, 60), true);
		assert!(tab > 12_000 && tab <= MOBILE_AUTO_CAP_MAX_KBPS, "{tab}");
		assert_eq!(mobile_auto_cap_kbps("h264", (3840, 2160, 60), true), MOBILE_AUTO_CAP_MAX_KBPS);
		assert_eq!(mobile_auto_cap_kbps("h264", (1280, 720, 30), true), 15_000);
	}

	#[test]
	fn path_change_picks_the_cap_of_the_new_path() {
		// Relayed with a 10 Mbit relay cap: 9 Mbit (under the direct cap).
		assert_eq!(path_cap_kbps(true, 10_000, 40_000), 9_000);
		// Direct: the relay's cap no longer applies.
		assert_eq!(path_cap_kbps(false, 10_000, 40_000), 40_000);
		// An uncapped relay: the direct cap.
		assert_eq!(path_cap_kbps(true, 0, 40_000), 40_000);
		// A tiny relay cap still leaves 2 Mbit; a direct cap under the relay's wins.
		assert_eq!(path_cap_kbps(true, 1_000, 40_000), 2_000);
		assert_eq!(path_cap_kbps(true, 50_000, 15_000), 15_000);
		assert_eq!(relay_cap_kbps(0), 0);
	}
}
