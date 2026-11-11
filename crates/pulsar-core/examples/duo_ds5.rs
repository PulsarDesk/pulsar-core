//! Hardware check for the Duo DualSense backend (`pulsar_core::input::DuoDs5`).
//!
//! Creates a DualSense through Duo's `DuoController.dll`, feeds it a neutral input state,
//! then plays the game's part: writes a known output report 0x02 to the pad's HID
//! interface, first with `WriteFile`, then with `HidD_SetOutputReport`, and prints what
//! the backend's output reader returns for each. The controller is removed at the end.
//!
//! Run ONLY on a machine with Duo installed, from an ELEVATED shell, inside the Duo session
//! that should own the pad (the device belongs to the session of the creating process):
//!
//! ```text
//! cargo run -p pulsar-core --example duo_ds5 -- [edge]
//! ```
//!
//! Expected: "parent: ROOT\VID_054C&PID_0CE6&DUOCONTROLLER&…", then for each write an
//! `Output` with flags 0f 14, motors (left 0x80, right 0x40), right trigger 01 20 c0, left
//! trigger 02 10 80 55, player LEDs 04, lightbar 11 22 33 (blue 00 after WriteFile: Duo
//! copies only 47 bytes there), and `PASS` lines. Exit code 1 on any mismatch.

#[cfg(windows)]
fn main() {
	std::process::exit(imp::run());
}

#[cfg(not(windows))]
fn main() {
	eprintln!("duo_ds5: Windows only");
}

#[cfg(windows)]
mod imp {
	use std::ffi::c_void;
	use std::io::Write;
	use std::os::windows::io::AsRawHandle;
	use std::sync::mpsc::Receiver;
	use std::time::{Duration, Instant};

	use pulsar_core::input::ds5::report::{neutral_usb_input, SetState, STATE_LEN};
	use pulsar_core::input::ds5::virtual_pad::Ds5HostEvent;
	use pulsar_core::input::{duo_available, duo_ds5_interfaces, DuoDs5};

	fn hex(b: &[u8]) -> String {
		b.iter()
			.map(|x| format!("{x:02x}"))
			.collect::<Vec<_>>()
			.join(" ")
	}

	/// The probe's known report (same as the `ds5::duo` unit tests).
	fn known_report() -> [u8; 48] {
		let mut out = [0u8; 48];
		out[0] = 0x02;
		out[1] = 0x0F; // rumble emulation | rumble-not-haptics | R trigger | L trigger
		out[2] = 0x14; // lightbar | player LEDs
		out[3] = 0x40; // right motor
		out[4] = 0x80; // left motor
		out[11] = 0x01; // R trigger: rigid
		out[12] = 0x20;
		out[13] = 0xC0;
		out[22] = 0x02; // L trigger: pulse
		out[23] = 0x10;
		out[24] = 0x80;
		out[25] = 0x55;
		out[44] = 0x04;
		out[45] = 0x11;
		out[46] = 0x22;
		out[47] = 0x33;
		out
	}

	/// The newest `Output` within `wait` (drains everything that arrived).
	fn last_output(rx: &Receiver<Ds5HostEvent>, wait: Duration) -> Option<SetState> {
		let end = Instant::now() + wait;
		let mut last = None;
		while let Some(left) = end.checked_duration_since(Instant::now()) {
			match rx.recv_timeout(left) {
				Ok(Ds5HostEvent::Output(s)) => {
					println!("  Output: {}", hex(&s.0[..47]));
					last = Some(s);
				}
				Ok(e) => println!("  other event: {e:?}"),
				Err(_) => break,
			}
		}
		last
	}

	fn check(tag: &str, got: Option<SetState>, blue: u8) -> bool {
		let r = known_report();
		let mut want = [0u8; 47];
		want.copy_from_slice(&r[1..48]);
		want[46] = blue;
		match got {
			Some(s) if s.0[..47] == want => {
				println!("PASS {tag}");
				true
			}
			Some(s) => {
				println!(
					"FAIL {tag}: got  {}\n            want {}",
					hex(&s.0[..47]),
					hex(&want)
				);
				false
			}
			None => {
				println!(
					"FAIL {tag}: no Output event (output section not found or reader not running)"
				);
				false
			}
		}
	}

	pub fn run() -> i32 {
		let edge = std::env::args().nth(1).as_deref() == Some("edge");
		if !duo_available() {
			println!("DuoController.dll not loadable (Duo not installed? set PULSAR_DUO_DIR)");
			return 1;
		}
		let (pad, rx) = match DuoDs5::start(edge) {
			Ok(p) => p,
			Err(e) => {
				println!("DuoDs5::start failed: {e} (elevated shell?)");
				return 1;
			}
		};
		println!("parent: {:?}", pad.parent_instance_id());
		let mut ok = pad.parent_instance_id().is_some();
		let mut state = [0u8; STATE_LEN];
		state.copy_from_slice(&neutral_usb_input()[1..]);
		println!("send_raw: {}", pad.send_raw(&mut state));
		println!("  after send (DLL-stamped seq/ts/battery): {}", hex(&state));

		let path = pad
			.parent_instance_id()
			.and_then(|p| {
				duo_ds5_interfaces(edge)
					.into_iter()
					.find(|(_, par)| par == p)
			})
			.map(|(path, _)| path);
		let Some(path) = path else {
			println!("FAIL: no HID interface for this pad");
			drop(pad);
			return 1;
		};
		println!("hid: {path}");
		let report = known_report();
		match std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(&path)
		{
			Ok(mut f) => {
				let _ = last_output(&rx, Duration::from_millis(200));
				println!("WriteFile {}: {:?}", hex(&report), f.write(&report));
				ok &= check(
					"WriteFile",
					last_output(&rx, Duration::from_millis(500)),
					0x00,
				);
				// Change one byte first so the second write is a visible change.
				let mut other = report;
				other[45] = 0x99;
				let _ = f.write(&other);
				let _ = last_output(&rx, Duration::from_millis(300));
				println!(
					"HidD_SetOutputReport: {:?}",
					set_output_report(f.as_raw_handle(), &report)
				);
				ok &= check(
					"HidD_SetOutputReport",
					last_output(&rx, Duration::from_millis(500)),
					0x33,
				);
			}
			Err(e) => {
				println!("FAIL: open {path}: {e}");
				ok = false;
			}
		}
		drop(pad);
		println!("removed; the ROOT devnode must be gone (pnputil /enum-devices /class HIDClass)");
		if ok {
			0
		} else {
			1
		}
	}

	/// `hid.dll!HidD_SetOutputReport`, loaded at runtime (no extra windows-sys feature).
	fn set_output_report(h: std::os::windows::io::RawHandle, report: &[u8]) -> Result<(), String> {
		use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
		type F = unsafe extern "system" fn(*mut c_void, *const c_void, u32) -> u8;
		let name: Vec<u16> = "hid.dll\0".encode_utf16().collect();
		unsafe {
			let m = LoadLibraryW(name.as_ptr());
			if m.is_null() {
				return Err("hid.dll".into());
			}
			let Some(p) = GetProcAddress(m, c"HidD_SetOutputReport".as_ptr().cast()) else {
				return Err("HidD_SetOutputReport missing".into());
			};
			let f: F = std::mem::transmute(p);
			if f(h, report.as_ptr().cast(), report.len() as u32) != 0 {
				Ok(())
			} else {
				Err(std::io::Error::last_os_error().to_string())
			}
		}
	}
}
