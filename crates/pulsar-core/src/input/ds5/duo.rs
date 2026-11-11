//! Platform-independent pieces of the Duo DualSense backend (`input/duo.rs` on Windows):
//! the shared output section's name and layout, the rumble-only fallback state, and the
//! "should pads go through Duo" decision. Kept pure so they are unit-tested everywhere.
//!
//! Duo's DualSense driver (UMDF) mirrors the game's output report 0x02 into a 4 KiB section
//! `Global\Duo_<parent instance id, '\' â†’ '_'>_output` and signals `<same>_output_event`.
//! `DuoController.dll` forwards only the two motor bytes to its feedback callback, so the
//! adaptive triggers and LEDs must be read from the section directly. Layout (live probe on
//! Duo 1.6, 2026-11-11):
//! - game used `WriteFile`: `shm[0..47]` = report bytes 0..46 verbatim (`shm[0]` = 0x02, the
//!   body starts at `shm[1]`, the lightbar's blue byte is cut off);
//! - game used `HidD_SetOutputReport`: `shm[0]` = 0x02, then the whole report from `shm[1]`
//!   (so `shm[1]` = 0x02 again and the body starts at `shm[2]`).

use super::report::{flag, SetState, SET_STATE_USED, USB_OUTPUT_ID};

/// Size of Duo's output section.
pub const SECTION_LEN: usize = 4096;
/// Bytes of the section the parser looks at (id + id + 47 body bytes, rounded up).
pub const SECTION_PREFIX: usize = 64;

/// `Global\Duo_<id>_output` for the controller's ROOT devnode instance id (the parent of
/// its HID interface devnode).
pub fn section_name(parent_instance_id: &str) -> String {
	format!(
		r"Global\Duo_{}_output",
		parent_instance_id.replace('\\', "_")
	)
}

/// The event Duo's driver sets after writing the output section.
pub fn event_name(parent_instance_id: &str) -> String {
	format!("{}_event", section_name(parent_instance_id))
}

/// The game's output state from the first bytes of Duo's output section; `None` while the
/// game hasn't written a 0x02 report yet (the section starts zeroed).
///
/// Ambiguity: a `WriteFile` report whose flag byte 0 is exactly 0x02 (only "use rumble, not
/// haptics") looks like the `HidD_SetOutputReport` layout. That flag set alone changes
/// nothing on the pad, so misreading it costs one shifted (harmless) state.
pub fn parse_output(shm: &[u8]) -> Option<SetState> {
	if shm.first() != Some(&USB_OUTPUT_ID) {
		return None;
	}
	let (start, len) = if shm.get(1) == Some(&USB_OUTPUT_ID) {
		(2, SET_STATE_USED)
	} else {
		// WriteFile path: 47 report bytes copied, so 46 body bytes (lightbar B missing).
		(1, SET_STATE_USED - 1)
	};
	let end = (start + len).min(shm.len());
	if end <= start {
		return None;
	}
	SetState::from_bytes(&shm[start..end])
}

/// A rumble-only state from `DuoController.dll`'s feedback callback (`fb[3]` strong/left,
/// `fb[4]` weak/right), used when the output section can't be opened.
pub fn rumble_state(strong: u8, weak: u8) -> SetState {
	let mut s = SetState::default();
	s.0[0] = flag::B0_RUMBLE_EMULATION | flag::B0_USE_RUMBLE_NOT_HAPTICS;
	s.0[2] = weak;
	s.0[3] = strong;
	s
}

/// True for a HID interface path of a Duo DualSense (`edge`: DualSense Edge).
pub fn is_duo_ds5_hid_path(path: &str, edge: bool) -> bool {
	let l = path.to_ascii_lowercase();
	let pid = if edge { "pid_0df2" } else { "pid_0ce6" };
	l.contains(&format!("{pid}&duocontroller"))
}

/// The controller devnode that appeared since `before` and nobody has claimed yet. Duo's
/// devices show up asynchronously (~1.5 s) after `CreateController`, so the caller polls.
pub fn pick_new_parent(before: &[String], now: &[String], claimed: &[String]) -> Option<String> {
	now.iter()
		.find(|p| {
			!before.iter().any(|b| b.eq_ignore_ascii_case(p))
				&& !claimed.iter().any(|c| c.eq_ignore_ascii_case(p))
		})
		.cloned()
}

/// Whether new pads should go through Duo's driver. `force` is `PULSAR_PAD_BACKEND`
/// (`duo` / `vigem`, anything else = auto). Auto: only inside a non-console session of a
/// machine with Duo installed and HID isolation on (`HKLM\SOFTWARE\Duo\HostHidIsolation`,
/// missing = 1). In the console session Duo's shim denies `DUOCONTROLLER` devices.
pub fn duo_wanted(
	force: Option<&str>,
	duo_installed: bool,
	remote_session: bool,
	hid_isolation: Option<u32>,
) -> bool {
	match force {
		Some("duo") => true,
		Some("vigem") => false,
		_ => duo_installed && remote_session && hid_isolation.unwrap_or(1) != 0,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The probe's known report: flags 0x0F/0x14, motors 0x40/0x80, R trigger rigid,
	/// L trigger pulse, player LED 0x04, lightbar 11 22 33.
	fn probe_report() -> [u8; 48] {
		let mut out = [0u8; 48];
		out[0] = 0x02;
		out[1] = 0x0F;
		out[2] = 0x14;
		out[3] = 0x40;
		out[4] = 0x80;
		out[11] = 0x01;
		out[12] = 0x20;
		out[13] = 0xC0;
		out[22] = 0x02;
		out[23] = 0x10;
		out[24] = 0x80;
		out[25] = 0x55;
		out[44] = 0x04;
		out[45] = 0x11;
		out[46] = 0x22;
		out[47] = 0x33;
		out
	}

	fn check_body(s: &SetState, blue: u8) {
		assert_eq!(s.0[0], 0x0F);
		assert_eq!(s.0[1], 0x14);
		assert_eq!(s.rumble(), Some((0x80, 0x40)), "(left/strong, right/weak)");
		assert!(s.has_triggers());
		assert_eq!(&s.0[10..13], &[0x01, 0x20, 0xC0], "right trigger");
		assert_eq!(&s.0[21..25], &[0x02, 0x10, 0x80, 0x55], "left trigger");
		assert_eq!(s.0[43], 0x04, "player LEDs");
		assert_eq!(&s.0[44..47], &[0x11, 0x22, blue], "lightbar");
		assert!(s.0[47..].iter().all(|&b| b == 0));
	}

	#[test]
	fn write_file_layout() {
		let r = probe_report();
		let mut shm = [0u8; SECTION_PREFIX];
		shm[..47].copy_from_slice(&r[..47]);
		// Whatever follows the copied bytes must not leak in as the blue byte.
		shm[47] = 0xEE;
		check_body(&parse_output(&shm).unwrap(), 0);
	}

	#[test]
	fn set_output_report_layout() {
		let r = probe_report();
		let mut shm = [0u8; SECTION_PREFIX];
		shm[0] = 0x02;
		shm[1..49].copy_from_slice(&r);
		check_body(&parse_output(&shm).unwrap(), 0x33);
	}

	#[test]
	fn empty_or_foreign_section() {
		assert_eq!(
			parse_output(&[0u8; SECTION_PREFIX]),
			None,
			"zeroed section = no report yet"
		);
		assert_eq!(parse_output(&[]), None);
		assert_eq!(parse_output(&[0x02]), None, "id only");
		let mut shm = [0u8; SECTION_PREFIX];
		shm[0] = 0x05;
		shm[1] = 0x0F;
		assert_eq!(parse_output(&shm), None);
		// Short buffers are zero-padded, not rejected.
		let s = parse_output(&[0x02, 0x0C, 0x14]).unwrap();
		assert_eq!(&s.0[..2], &[0x0C, 0x14]);
	}

	#[test]
	fn names_from_parent_id() {
		let id = r"ROOT\VID_054C&PID_0CE6&DUOCONTROLLER&00FB7C9D000000000000744000000001\0000";
		assert_eq!(
			section_name(id),
			r"Global\Duo_ROOT_VID_054C&PID_0CE6&DUOCONTROLLER&00FB7C9D000000000000744000000001_0000_output"
		);
		assert_eq!(
			event_name(id),
			r"Global\Duo_ROOT_VID_054C&PID_0CE6&DUOCONTROLLER&00FB7C9D000000000000744000000001_0000_output_event"
		);
	}

	#[test]
	fn rumble_fallback_state() {
		let s = rumble_state(200, 30);
		assert_eq!(s.rumble(), Some((200, 30)));
		assert!(!s.has_triggers());
	}

	#[test]
	fn hid_path_match() {
		let p = r"\\?\hid#vid_054c&pid_0ce6&duocontroller&00fb7c9d#1&2d595ca7&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
		assert!(is_duo_ds5_hid_path(p, false));
		assert!(!is_duo_ds5_hid_path(p, true));
		assert!(
			!is_duo_ds5_hid_path(r"\\?\hid#vid_054c&pid_0ce6&mi_03#7&1", false),
			"a real DualSense"
		);
		assert!(is_duo_ds5_hid_path(
			r"\\?\HID#VID_054C&PID_0DF2&DUOCONTROLLER&1#2",
			true
		));
	}

	#[test]
	fn new_parent_pick() {
		let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
		let before = s(&[r"ROOT\A\0000"]);
		assert_eq!(pick_new_parent(&before, &before, &[]), None, "not up yet");
		let now = s(&[r"ROOT\A\0000", r"ROOT\B\0000", r"ROOT\C\0000"]);
		assert_eq!(
			pick_new_parent(&before, &now, &s(&[r"root\b\0000"])).as_deref(),
			Some(r"ROOT\C\0000"),
			"claimed skipped"
		);
		assert_eq!(
			pick_new_parent(&before, &now, &[]).as_deref(),
			Some(r"ROOT\B\0000")
		);
	}

	#[test]
	fn backend_selection() {
		assert!(duo_wanted(Some("duo"), false, false, Some(0)), "forced on");
		assert!(
			!duo_wanted(Some("vigem"), true, true, Some(1)),
			"forced off"
		);
		assert!(
			duo_wanted(None, true, true, None),
			"Duo seat, isolation default on"
		);
		assert!(duo_wanted(Some(""), true, true, Some(1)));
		assert!(!duo_wanted(None, true, false, Some(1)), "console session");
		assert!(
			!duo_wanted(None, false, true, Some(1)),
			"no Duo (plain RDP)"
		);
		assert!(
			!duo_wanted(None, true, true, Some(0)),
			"isolation off: ViGEm/usbip are visible"
		);
	}
}
