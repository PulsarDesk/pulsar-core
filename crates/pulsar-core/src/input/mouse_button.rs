//! Mouse button ids carried by [`crate::service::InputEvent::PointerButton`], and their
//! per-OS codes. One table for every capture and injection path, so a side button means
//! the same thing everywhere.
//!
//! `0`..=`2` are left / right / middle (every host knows them). `3` / `4` are the side
//! buttons, X1 = back and X2 = forward (Windows `XBUTTON1` / `XBUTTON2`, Linux `BTN_SIDE` /
//! `BTN_EXTRA`, macOS button numbers 3 / 4, X11 buttons 8 / 9). Hosts older than
//! [`crate::service::media::FEAT_MOUSE_X`] turned every id they did not know into a LEFT
//! click, so a client sends `3` / `4` only to hosts that advertise that feature. Current hosts
//! ignore any id above [`FORWARD`].

pub const LEFT: u8 = 0;
pub const RIGHT: u8 = 1;
pub const MIDDLE: u8 = 2;
/// X1, the "back" side button.
pub const BACK: u8 = 3;
/// X2, the "forward" side button.
pub const FORWARD: u8 = 4;

/// Whether a host should act on this button id at all (unknown ids are ignored, never
/// folded into a left click).
pub const fn is_known(button: u8) -> bool {
	button <= FORWARD
}

/// Whether this id needs [`crate::service::media::FEAT_MOUSE_X`] on the host.
pub const fn is_side(button: u8) -> bool {
	matches!(button, BACK | FORWARD)
}

/// Whether a host injector drops this edge: the release of a side button it never pressed.
/// Clients release every button whenever they let go of control (blur, Ctrl+Alt+Z, free-cursor
/// hover-out), and a stray side-button release is not inert: Windows' DefWindowProc turns
/// WM_XBUTTONUP into WM_APPCOMMAND BROWSER_BACKWARD / FORWARD and macOS apps navigate on the
/// other-mouse-up, so the app under the host cursor would go back and forward. Left / right /
/// middle releases still pass (inert when unpaired, and a press may have come in through
/// another path such as the secure-desktop helper).
pub const fn drop_unpaired_release(button: u8, down: bool, was_held: bool) -> bool {
	!down && !was_held && is_side(button)
}

/// Linux evdev `BTN_*` code of a button (what a uinput mouse emits).
pub const fn to_evdev(button: u8) -> Option<u16> {
	Some(match button {
		LEFT => 0x110,    // BTN_LEFT
		RIGHT => 0x111,   // BTN_RIGHT
		MIDDLE => 0x112,  // BTN_MIDDLE
		BACK => 0x113,    // BTN_SIDE
		FORWARD => 0x114, // BTN_EXTRA
		_ => return None,
	})
}

/// Button id of a Linux evdev `BTN_*` code read from a physical mouse. Mice report their
/// side buttons either as `BTN_SIDE` / `BTN_EXTRA` (most) or `BTN_BACK` / `BTN_FORWARD`
/// (some); both pairs fold onto back / forward, as SDL and X11 do.
pub const fn from_evdev(code: u16) -> Option<u8> {
	Some(match code {
		0x110 => LEFT,
		0x111 => RIGHT,
		0x112 => MIDDLE,
		0x113 | 0x116 => BACK,    // BTN_SIDE, BTN_BACK
		0x114 | 0x115 => FORWARD, // BTN_EXTRA, BTN_FORWARD
		_ => return None,
	})
}

/// Every evdev code [`from_evdev`] reads as a mouse button (a held-key flush must not send
/// these as keyboard keys).
pub const fn is_evdev_button(code: u16) -> bool {
	from_evdev(code).is_some()
}

/// X11 core-protocol button number (8 = back, 9 = forward, as the X server numbers the
/// evdev side buttons).
pub const fn to_x11(button: u8) -> Option<u8> {
	Some(match button {
		LEFT => 1,
		MIDDLE => 2,
		RIGHT => 3,
		BACK => 8,
		FORWARD => 9,
		_ => return None,
	})
}

/// Windows `XBUTTON1` / `XBUTTON2` (the `mouseData` of `MOUSEEVENTF_XDOWN` / `XUP`, the high
/// word of `WM_XBUTTON*`'s wParam) of a side button.
pub const fn to_win_xbutton(button: u8) -> Option<u16> {
	match button {
		BACK => Some(1),
		FORWARD => Some(2),
		_ => None,
	}
}

/// Button id of a Windows `XBUTTON1` / `XBUTTON2`.
pub const fn from_win_xbutton(x: u16) -> Option<u8> {
	match x {
		1 => Some(BACK),
		2 => Some(FORWARD),
		_ => None,
	}
}

/// macOS `kCGMouseEventButtonNumber` of a button (2 = center, 3 / 4 = the side buttons).
pub const fn to_mac_number(button: u8) -> Option<i64> {
	Some(match button {
		LEFT => 0,
		RIGHT => 1,
		MIDDLE => 2,
		BACK => 3,
		FORWARD => 4,
		_ => return None,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn evdev_round_trips_and_folds_back_forward() {
		for b in 0..=FORWARD {
			assert_eq!(from_evdev(to_evdev(b).unwrap()), Some(b));
		}
		assert_eq!(to_evdev(5), None);
		assert_eq!(from_evdev(0x116), Some(BACK)); // BTN_BACK
		assert_eq!(from_evdev(0x115), Some(FORWARD)); // BTN_FORWARD
		assert_eq!(from_evdev(0x117), None); // BTN_TASK
		assert!(is_evdev_button(0x110) && !is_evdev_button(30));
	}

	#[test]
	fn only_unpaired_side_releases_are_dropped() {
		assert!(drop_unpaired_release(BACK, false, false));
		assert!(drop_unpaired_release(FORWARD, false, false));
		assert!(!drop_unpaired_release(BACK, false, true));
		assert!(!drop_unpaired_release(BACK, true, false));
		assert!(!drop_unpaired_release(LEFT, false, false));
	}

	#[test]
	fn other_os_codes() {
		assert_eq!(to_x11(BACK), Some(8));
		assert_eq!(to_x11(FORWARD), Some(9));
		assert_eq!(to_x11(RIGHT), Some(3));
		assert_eq!(to_x11(7), None);
		assert_eq!(to_win_xbutton(BACK), Some(1));
		assert_eq!(from_win_xbutton(2), Some(FORWARD));
		assert_eq!(to_win_xbutton(LEFT), None);
		assert_eq!(to_mac_number(FORWARD), Some(4));
		assert_eq!(to_mac_number(9), None);
	}

	#[test]
	fn only_ids_up_to_forward_are_known() {
		assert!(is_known(0) && is_known(4));
		assert!(!is_known(5) && !is_known(255));
		assert!(is_side(3) && is_side(4) && !is_side(2));
	}
}
