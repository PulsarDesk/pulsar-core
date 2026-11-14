//! Which display an AVFoundation capture device is (macOS host).
//!
//! The macOS host captures with ffmpeg's `avfoundation` input, whose device list puts the
//! cameras first and then one `Capture screen <n>` per active display, `<n>` being the
//! display's position in `CGGetActiveDisplayList`. Pointer, pen and touch injection must
//! land on that display, not on the main one. The listing parse is pure (tested on every
//! platform); [`screen_display_id`] resolves the CoreGraphics display on macOS.

/// The `<n>` of `Capture screen <n>` for the video device `[device]` of an
/// `ffmpeg -f avfoundation -list_devices true -i ""` listing (its stderr). `None` when the
/// device is not a screen (a camera) or not listed.
pub fn screen_index(listing: &str, device: u32) -> Option<usize> {
	let mut in_video = false;
	for line in listing.lines() {
		let lower = line.to_ascii_lowercase();
		if lower.contains("avfoundation") && lower.contains("devices") {
			in_video = lower.contains("video");
			continue;
		}
		if !in_video {
			continue;
		}
		// `[AVFoundation indev @ 0x…] [<idx>] <name>`: the LAST bracket pair is the index.
		let Some(open) = line.rfind('[') else { continue };
		let rest = &line[open + 1..];
		let Some(close) = rest.find(']') else { continue };
		if rest[..close].trim().parse::<u32>().ok() != Some(device) {
			continue;
		}
		let name = rest[close + 1..].trim();
		return name.strip_prefix("Capture screen ")?.trim().parse().ok();
	}
	None
}

/// The CoreGraphics id of the `n`-th active display (the order avfoundation numbers its
/// screens in).
#[cfg(target_os = "macos")]
pub fn screen_display_id(n: usize) -> Option<u32> {
	core_graphics::display::CGDisplay::active_displays().ok()?.get(n).copied()
}

#[cfg(test)]
mod tests {
	use super::*;

	const LISTING: &str = "[AVFoundation indev @ 0x7f8] AVFoundation video devices:\n\
[AVFoundation indev @ 0x7f8] [0] FaceTime HD Camera\n\
[AVFoundation indev @ 0x7f8] [1] Capture screen 0\n\
[AVFoundation indev @ 0x7f8] [2] Capture screen 1\n\
[AVFoundation indev @ 0x7f8] AVFoundation audio devices:\n\
[AVFoundation indev @ 0x7f8] [0] MacBook Pro Microphone\n";

	#[test]
	fn screens_are_found_after_the_cameras() {
		assert_eq!(screen_index(LISTING, 1), Some(0));
		assert_eq!(screen_index(LISTING, 2), Some(1));
		assert_eq!(screen_index(LISTING, 0), None); // the camera
		assert_eq!(screen_index(LISTING, 3), None);
		assert_eq!(screen_index("", 1), None);
	}
}
