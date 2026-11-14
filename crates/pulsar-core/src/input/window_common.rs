//! Pure, platform-independent pieces shared by the per-WINDOW injectors
//! (`window_x11.rs` on Linux, `window_mac.rs` on macOS). Kept free of any OS call
//! so they compile — and their unit tests run — on every target, including the
//! Windows dev box where the X11/macOS backends themselves can't be exercised.

/// Map a normalized (0..1) pointer onto a `w`×`h` area (pixels or points). The far
/// edge maps to `size - 1` so a click at 1.0 stays inside the window.
#[allow(dead_code)]
pub(crate) fn norm_to_area(x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
	let span = |s: f64| (s - 1.0).max(0.0);
	(x.clamp(0.0, 1.0) * span(w), y.clamp(0.0, 1.0) * span(h))
}

/// Clamp a point to a `w`×`h` area (the cached pointer after a relative move).
#[allow(dead_code)]
pub(crate) fn clamp_to_area(x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
	(x.clamp(0.0, (w - 1.0).max(0.0)), y.clamp(0.0, (h - 1.0).max(0.0)))
}

/// Carries fractional wheel notches between calls (browser wheel pixels, 100 per
/// notch), like the global injectors do, so precision scroll isn't lost to rounding.
/// `push` returns whole notches: `v` positive = scroll DOWN, `h` positive = RIGHT.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct ScrollAcc {
	v: f64,
	h: f64,
}

#[allow(dead_code)]
impl ScrollAcc {
	pub(crate) fn push(&mut self, dx: f64, dy: f64) -> (i32, i32) {
		self.v += dy / 100.0;
		self.h += dx / 100.0;
		let v = self.v.trunc() as i32;
		let h = self.h.trunc() as i32;
		self.v -= v as f64;
		self.h -= h as f64;
		(v, h)
	}
}

/// X11 core-protocol button number for our button id (0=left, 1=right, 2=middle).
#[allow(dead_code)]
pub(crate) fn x11_button(button: u8) -> u8 {
	match button {
		1 => 3,
		2 => 2,
		_ => 1,
	}
}

/// X11 `KeyButMask` bit that a held button sets in an event's `state`.
#[allow(dead_code)]
pub(crate) fn x11_button_mask(button: u8) -> u16 {
	match x11_button(button) {
		1 => 1 << 8, // Button1
		2 => 1 << 9, // Button2
		_ => 1 << 10, // Button3
	}
}

/// X11 modifier `state` bit an evdev modifier key asserts while held (standard
/// XKB modifier map: Shift, Control, Mod1 = Alt, Mod4 = Super, Mod5 = AltGr).
/// Caps Lock is a toggle and handled separately.
#[allow(dead_code)]
pub(crate) fn x11_modifier_mask(code: u32) -> Option<u16> {
	Some(match code {
		42 | 54 => 1 << 0,  // L/R Shift   → ShiftMask
		29 | 97 => 1 << 2,  // L/R Ctrl    → ControlMask
		56 => 1 << 3,       // Left Alt    → Mod1Mask
		100 => 1 << 7,      // Right Alt (AltGr) → Mod5Mask
		125 | 126 => 1 << 6, // L/R Super  → Mod4Mask
		_ => return None,
	})
}

/// The X server keycode of an evdev keycode: every evdev/libinput-backed X server
/// (Xorg + `xf86-input-evdev`/`libinput`, XWayland) offsets them by 8.
#[allow(dead_code)]
pub(crate) fn x11_keycode(evdev: u32) -> Option<u8> {
	let k = evdev.checked_add(8)?;
	(8..=255).contains(&k).then_some(k as u8)
}

/// The X keysym that types `c`: control characters get their function keysyms,
/// Latin-1 maps 1:1, everything else uses the Unicode keysym range (`0x0100_0000 |
/// codepoint`, understood by Xlib/XKB since X11R6.9).
#[allow(dead_code)]
pub(crate) fn char_to_keysym(c: char) -> Option<u32> {
	let cp = c as u32;
	Some(match c {
		'\n' | '\r' => 0xff0d, // XK_Return
		'\t' => 0xff09,        // XK_Tab
		'\u{8}' => 0xff08,     // XK_BackSpace
		'\u{1b}' => 0xff1b,    // XK_Escape
		'\u{7f}' => 0xffff,    // XK_Delete
		_ if cp < 0x20 => return None,
		_ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => cp,
		_ => 0x0100_0000 | cp,
	})
}

/// Find `keysym` in a keyboard mapping (`syms_per` keysyms per keycode, starting at
/// `first`). Returns the keycode and whether Shift is needed (found on level 1).
/// Only the first two levels (no group switching) are considered — the same subset
/// `XKeysymToKeycode` + Shift covers.
#[allow(dead_code)]
pub(crate) fn find_keysym(map: &[u32], syms_per: usize, first: u8, keysym: u32) -> Option<(u8, bool)> {
	if syms_per == 0 {
		return None;
	}
	for (i, syms) in map.chunks(syms_per).enumerate() {
		let code = first as usize + i;
		if code > 255 {
			break;
		}
		// A keycode with only level 0 set (e.g. "a" with no "A") types the same
		// char regardless of Shift; a 1-level match means "no Shift".
		for (level, &s) in syms.iter().take(2).enumerate() {
			if s == keysym {
				return Some((code as u8, level == 1));
			}
		}
	}
	None
}

/// Keycodes with no keysym at all — free slots a temporary mapping can borrow to
/// type a character the layout lacks (the same trick `xdotool type` uses).
#[allow(dead_code)]
pub(crate) fn free_keycodes(map: &[u32], syms_per: usize, first: u8, max: usize) -> Vec<u8> {
	if syms_per == 0 {
		return Vec::new();
	}
	map.chunks(syms_per)
		.enumerate()
		.filter(|(_, syms)| syms.iter().all(|&s| s == 0))
		.map(|(i, _)| first as usize + i)
		.filter(|&c| (8..=255).contains(&c))
		.map(|c| c as u8)
		.take(max)
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_normalized_pointer_inside_area() {
		assert_eq!(norm_to_area(0.0, 0.0, 800.0, 600.0), (0.0, 0.0));
		assert_eq!(norm_to_area(1.0, 1.0, 800.0, 600.0), (799.0, 599.0));
		assert_eq!(norm_to_area(2.0, -1.0, 800.0, 600.0), (799.0, 0.0));
		assert_eq!(norm_to_area(0.5, 0.5, 0.0, 0.0), (0.0, 0.0));
		assert_eq!(clamp_to_area(-5.0, 900.0, 800.0, 600.0), (0.0, 599.0));
	}

	#[test]
	fn scroll_accumulates_fractional_notches() {
		let mut a = ScrollAcc::default();
		assert_eq!(a.push(0.0, 40.0), (0, 0));
		assert_eq!(a.push(0.0, 70.0), (1, 0));
		assert_eq!(a.push(-250.0, -10.0), (0, -2));
	}

	#[test]
	fn x11_buttons_and_masks() {
		assert_eq!(x11_button(0), 1);
		assert_eq!(x11_button(1), 3);
		assert_eq!(x11_button(2), 2);
		assert_eq!(x11_button_mask(0), 0x100);
		assert_eq!(x11_button_mask(1), 0x400);
		assert_eq!(x11_modifier_mask(42), Some(1));
		assert_eq!(x11_modifier_mask(29), Some(4));
		assert_eq!(x11_modifier_mask(30), None);
	}

	#[test]
	fn x11_keycode_offsets_evdev_by_8() {
		assert_eq!(x11_keycode(30), Some(38)); // KEY_A → X keycode 38
		assert_eq!(x11_keycode(248), None);
	}

	#[test]
	fn keysyms_for_chars() {
		assert_eq!(char_to_keysym('a'), Some(0x61));
		assert_eq!(char_to_keysym('é'), Some(0xe9));
		assert_eq!(char_to_keysym('ç'), Some(0xe7));
		assert_eq!(char_to_keysym('ş'), Some(0x0100_015f));
		assert_eq!(char_to_keysym('\n'), Some(0xff0d));
		assert_eq!(char_to_keysym('\u{1}'), None);
	}

	#[test]
	fn finds_keysym_level_and_free_slots() {
		// keycodes 8..=10, 2 syms each: 8 = (a, A), 9 = empty, 10 = (1, !)
		let map = [0x61, 0x41, 0, 0, 0x31, 0x21];
		assert_eq!(find_keysym(&map, 2, 8, 0x41), Some((8, true)));
		assert_eq!(find_keysym(&map, 2, 8, 0x31), Some((10, false)));
		assert_eq!(find_keysym(&map, 2, 8, 0x99), None);
		assert_eq!(free_keycodes(&map, 2, 8, 4), vec![9]);
	}
}
