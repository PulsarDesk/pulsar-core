//! Operating-point ladder (adaptive streaming Phase 1): the resolution × fps points a
//! session may run at, each with the **minimum bitrate at which it still looks clean**.
//! "Fewer clean pixels beat many broken ones" — when the path's budget drops under a
//! point's minimum, the controller steps down a point instead of starving the encoder.

use crate::pipeline::VCodec;

/// One rung of the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Point {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	/// Below this bitrate (kbit/s) the point is considered too starved to be clean.
	pub min_kbps: u32,
}

impl Point {
	pub fn label(&self) -> String {
		format!("{}p{}", self.height, self.fps)
	}
}

/// Reference minimums for H.264 at the canonical heights (from the design doc). Other
/// heights/fps derive from the same bits-per-pixel-per-second; HEVC/AV1 need ~30 % less.
const REF_H264: &[(u32, u32, u32)] = &[
	// (height, fps, min_kbps)
	(1080, 60, 8000),
	(1080, 30, 5000),
	(720, 60, 4000),
	(720, 30, 2500),
	(540, 30, 1200),
	(360, 30, 600),
];

/// Codec efficiency relative to H.264 (a lower factor = fewer bits for the same look).
fn codec_factor(codec: VCodec) -> f32 {
	match codec {
		VCodec::H264 => 1.0,
		VCodec::H265 => 0.7,
		VCodec::Av1 => 0.65,
	}
}

/// Minimum bitrate for an arbitrary `w × h @ fps` (H.264 baseline), by bits/pixel/s
/// interpolated from the reference table: ~0.064 bpp at 60 fps, ~0.085 bpp at ≤ 30 fps.
fn min_kbps_for(w: u32, h: u32, fps: u32) -> u32 {
	let bpp = if fps >= 50 { 0.064 } else { 0.085 };
	let bits = w as f64 * h as f64 * fps as f64 * bpp;
	((bits / 1000.0).round() as u32).max(200)
}

/// Round a width to a multiple of 16 (encoder-friendly), at least 160.
fn even16(w: f64) -> u32 {
	(((w / 16.0).round() as u32) * 16).max(160)
}

/// Build the ladder for a host whose native capture is `native_w × native_h @ native_fps`
/// (0 = unknown → 1920×1080@60), highest quality first. Heights above the native one are
/// dropped; the native point is always the top rung; widths follow the native aspect
/// ratio; fps rungs are `native_fps` (if ≥ 50) and 30.
pub fn build(codec: VCodec, native_w: u32, native_h: u32, native_fps: u32) -> Vec<Point> {
	let (nw, nh, nfps) = (
		if native_w == 0 { 1920 } else { native_w },
		if native_h == 0 { 1080 } else { native_h },
		if native_fps == 0 { 60 } else { native_fps },
	);
	let aspect = nw as f64 / nh as f64;
	let factor = codec_factor(codec) as f64;
	let mut heights: Vec<u32> = vec![nh];
	for &(h, _, _) in REF_H264 {
		if h < nh && !heights.contains(&h) {
			heights.push(h);
		}
	}
	let fps_rungs: Vec<u32> = if nfps >= 50 { vec![nfps, 30] } else { vec![nfps.max(1)] };
	let mut out = Vec::new();
	for &h in &heights {
		let w = if h == nh { nw } else { even16(h as f64 * aspect) };
		for &fps in &fps_rungs {
			// The two lowest rungs only exist at 30 fps (the reference table has no 60 fps
			// there — motion is cheap at that size, spend the bits on pixels).
			if h <= 540 && fps != fps_rungs[fps_rungs.len() - 1] {
				continue;
			}
			// Prefer the reference table where it applies (exact height + fps).
			let reference = REF_H264
				.iter()
				.find(|&&(rh, rf, _)| rh == h && rf == fps)
				.map(|&(_, _, k)| k);
			let base = reference.unwrap_or_else(|| min_kbps_for(w, h, fps));
			out.push(Point {
				width: w,
				height: h,
				fps,
				min_kbps: ((base as f64 * factor).round() as u32).max(200),
			});
		}
	}
	// Highest quality first = the rung that needs the most bits (so `pick`'s first fit is
	// the best the budget buys); ties by pixels×fps.
	out.sort_by(|a, b| {
		let ka = a.width as u64 * a.height as u64 * a.fps as u64;
		let kb = b.width as u64 * b.height as u64 * b.fps as u64;
		b.min_kbps.cmp(&a.min_kbps).then(kb.cmp(&ka))
	});
	out.dedup();
	out
}

/// Index of the highest point whose minimum fits `budget_kbps × margin` (0.85 in the
/// design). Falls back to the lowest rung when nothing fits.
pub fn pick(points: &[Point], budget_kbps: u32, margin: f32) -> usize {
	let budget = budget_kbps as f32 * margin;
	points
		.iter()
		.position(|p| p.min_kbps as f32 <= budget)
		.unwrap_or(points.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reference_ladder_for_1080p60_h264() {
		let l = build(VCodec::H264, 1920, 1080, 60);
		let got: Vec<(String, u32)> = l.iter().map(|p| (p.label(), p.min_kbps)).collect();
		assert_eq!(
			got,
			vec![
				("1080p60".to_string(), 8000),
				("1080p30".to_string(), 5000),
				("720p60".to_string(), 4000),
				("720p30".to_string(), 2500),
				("540p30".to_string(), 1200),
				("360p30".to_string(), 600),
			]
		);
		assert_eq!(l[2].width, 1280, "16:9 widths follow the aspect");
		assert_eq!(l[4].width, 960);
		assert_eq!(l[5].width, 640);
	}

	#[test]
	fn hevc_needs_fewer_bits_and_unknown_native_defaults_to_1080p60() {
		let h = build(VCodec::H265, 0, 0, 0);
		assert_eq!(h[0].label(), "1080p60");
		assert_eq!(h[0].min_kbps, 5600);
		assert_eq!(h[5].min_kbps, 420);
		let a = build(VCodec::Av1, 0, 0, 0);
		assert!(a[0].min_kbps < h[0].min_kbps);
	}

	#[test]
	fn native_above_1080_gets_its_own_top_rung_and_ultrawide_keeps_aspect() {
		let l = build(VCodec::H264, 2560, 1440, 144);
		assert_eq!(l[0].label(), "1440p144");
		assert!(l[0].min_kbps > 8000, "{}", l[0].min_kbps);
		assert_eq!(l[1].label(), "1080p144", "ordered by the bits a rung needs");
		assert_eq!(l[1].width, 1920);
		assert_eq!(l[2].label(), "1440p30");
		let uw = build(VCodec::H264, 3440, 1440, 60);
		let p720 = uw.iter().find(|p| p.height == 720).unwrap();
		assert_eq!(p720.width, 1728, "3440/1440 × 720 = 1720 → nearest multiple of 16");
	}

	#[test]
	fn low_native_fps_has_a_single_fps_rung() {
		let l = build(VCodec::H264, 1280, 720, 30);
		let labels: Vec<String> = l.iter().map(Point::label).collect();
		assert_eq!(labels, vec!["720p30", "540p30", "360p30"]);
	}

	#[test]
	fn pick_uses_the_margin_and_falls_back_to_the_lowest_rung() {
		let l = build(VCodec::H264, 1920, 1080, 60);
		assert_eq!(l[pick(&l, 10_000, 0.85)].label(), "1080p60");
		assert_eq!(l[pick(&l, 9_000, 0.85)].label(), "1080p30", "8000 > 9000×0.85");
		assert_eq!(l[pick(&l, 2_000, 0.85)].label(), "540p30");
		assert_eq!(l[pick(&l, 300, 0.85)].label(), "360p30");
	}
}
