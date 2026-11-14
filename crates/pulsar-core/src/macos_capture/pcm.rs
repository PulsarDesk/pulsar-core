//! PCM reshaping for the ScreenCaptureKit audio output. SCK hands out 32-bit float audio,
//! planar (one buffer per channel) by default; the host's encoders take interleaved stereo
//! f32. Pure, so it builds and is unit-tested on every target.

/// Interleave `frames` frames of planar channels (`planes[c][i]`) into `out` (cleared first).
/// A plane shorter than `frames` is padded with silence.
pub fn interleave_planar(planes: &[&[f32]], frames: usize, out: &mut Vec<f32>) {
	out.clear();
	out.reserve(frames * planes.len());
	for i in 0..frames {
		for p in planes {
			out.push(p.get(i).copied().unwrap_or(0.0));
		}
	}
}

/// Turn interleaved `channels`-wide f32 PCM into interleaved stereo in `out` (cleared first):
/// mono is doubled, stereo copied, wider layouts keep front left/right and fold the centre in
/// at −3 dB (the order SCK and Core Audio use: L R C …).
pub fn to_stereo(src: &[f32], channels: usize, out: &mut Vec<f32>) {
	out.clear();
	if channels == 0 {
		return;
	}
	let frames = src.len() / channels;
	out.reserve(frames * 2);
	for f in src.chunks_exact(channels) {
		match channels {
			1 => {
				out.push(f[0]);
				out.push(f[0]);
			}
			2 => out.extend_from_slice(f),
			_ => {
				let c = f[2] * std::f32::consts::FRAC_1_SQRT_2;
				out.push((f[0] + c).clamp(-1.0, 1.0));
				out.push((f[1] + c).clamp(-1.0, 1.0));
			}
		}
	}
}

/// Signed 16-bit interleaved samples as f32 in −1..1 (an integer SCK/Core Audio format).
pub fn i16_to_f32(src: &[i16], out: &mut Vec<f32>) {
	out.clear();
	out.extend(src.iter().map(|s| *s as f32 / 32768.0));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn interleaves_two_planes() {
		let l = [1.0, 2.0, 3.0];
		let r = [-1.0, -2.0];
		let mut out = Vec::new();
		interleave_planar(&[&l, &r], 3, &mut out);
		assert_eq!(out, [1.0, -1.0, 2.0, -2.0, 3.0, 0.0]);
	}

	#[test]
	fn mono_is_doubled_and_stereo_copied() {
		let mut out = Vec::new();
		to_stereo(&[0.5, -0.5], 1, &mut out);
		assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
		to_stereo(&[0.1, 0.2, 0.3, 0.4], 2, &mut out);
		assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
	}

	#[test]
	fn surround_keeps_the_fronts_with_the_centre_folded_in() {
		let mut out = Vec::new();
		to_stereo(&[0.2, 0.1, 0.0, 0.9, 0.9, 0.9], 6, &mut out);
		assert_eq!(out, [0.2, 0.1]);
		to_stereo(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], 6, &mut out);
		assert!((out[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
		assert_eq!(out[0], out[1]);
	}

	#[test]
	fn zero_channels_give_nothing() {
		let mut out = vec![1.0];
		to_stereo(&[1.0, 2.0], 0, &mut out);
		assert!(out.is_empty());
	}

	#[test]
	fn i16_scales_to_unit_range() {
		let mut out = Vec::new();
		i16_to_f32(&[0, 16384, -32768], &mut out);
		assert_eq!(out, [0.0, 0.5, -1.0]);
	}
}
