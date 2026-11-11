//! Windows host-side virtual gamepad through **Duo's own controller driver** (DuoStream's
//! `DuoController.dll`, loaded at runtime from the user's Duo install — never shipped).
//!
//! Why: Duo (multi-seat streaming, one RDP session per player) shims every process with
//! `DuoAcRes64.dll`, whose `NtCreateFile` hook classifies each `hid#` device by its device
//! tree. A device whose tree contains `DUOCONTROLLER` (or `XBOXGIPSYNTHETIC` / `TERMINPUT_BUS`)
//! is "remote"; anything else — including every ViGEmBus pad — is "console". In a non-console
//! session with `HKLM\SOFTWARE\Duo\HostHidIsolation` ≠ 0 (default 1) console devices are
//! DENIED, so a ViGEm pad created by the Pulsar host inside a Duo session is invisible to that
//! session's Steam and games: keyboard/mouse work, the pad does not. Creating the pad through
//! `DuoController.dll` yields a `ROOT\VID_…&DUOCONTROLLER&…` device that the session accepts.
//! (Static analysis of Duo 1.5.x/1.6 binaries, 2026-11-11; see pulsar-desktop docs.)
//!
//! API (all HRESULT, x64 cdecl/win64):
//! - `DuoController_Initialize()` / `DuoController_Uninitialize()`
//! - `DuoController_CreateController(type, feedback_cb, ctx, &handle)` — type 0 = Xbox One HID,
//!   1 = DualShock 4, 2 = DualSense, 3 = DualSense Edge. Needs an ELEVATED token and
//!   `DuoController.inf` next to the DLL. The device belongs to the session of the process
//!   that loaded the DLL.
//! - `DuoController_SendReport(handle, report*)` — a per-type struct (not a raw HID report).
//! - `DuoController_RemoveController(handle)`.
//! - Feedback: `cb(handle, const u8 fb[8], ctx)` on a DLL thread; fb[3] = strong, fb[4] = weak.
//!
//! The DLL keeps a global handle array without a lock, so every call goes through one mutex.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use super::{ds4_report_fields, button, GamepadKind, GamepadState, ResolvedTarget, RumbleReader, VirtualGamepad};

type Hresult = i32;
type FeedbackCb = unsafe extern "C" fn(h: *mut c_void, fb: *const u8, ctx: *mut c_void);
type FnVoid = unsafe extern "C" fn() -> Hresult;
type FnCreate = unsafe extern "C" fn(kind: u32, cb: Option<FeedbackCb>, ctx: *mut c_void, out: *mut *mut c_void) -> Hresult;
type FnSend = unsafe extern "C" fn(h: *mut c_void, report: *mut u8) -> Hresult;
type FnRemove = unsafe extern "C" fn(h: *mut c_void) -> Hresult;

struct Api {
	create: FnCreate,
	send: FnSend,
	remove: FnRemove,
}

/// The loaded library (`None` = not available here). Loaded + initialized once per process.
static API: OnceLock<Option<Api>> = OnceLock::new();
/// Serializes every DLL call (the DLL's handle array is not thread-safe).
static CALL: Mutex<()> = Mutex::new(());
/// Rumble channels by device handle (the feedback callback runs on a DLL thread).
static RUMBLE: OnceLock<Mutex<HashMap<usize, mpsc::SyncSender<(u8, u8)>>>> = OnceLock::new();

fn rumble_map() -> &'static Mutex<HashMap<usize, mpsc::SyncSender<(u8, u8)>>> {
	RUMBLE.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe extern "C" fn on_feedback(h: *mut c_void, fb: *const u8, _ctx: *mut c_void) {
	if fb.is_null() {
		return;
	}
	let fb = unsafe { std::slice::from_raw_parts(fb, 8) };
	if let Some(tx) = rumble_map().lock().unwrap_or_else(|e| e.into_inner()).get(&(h as usize)) {
		let _ = tx.try_send((fb[3], fb[4]));
	}
}

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `HKLM\<key>\<value>` as a string (REG_SZ / REG_EXPAND_SZ, env vars NOT expanded).
fn reg_string(key: &str, value: &str) -> Option<String> {
	use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_RT_REG_EXPAND_SZ, RRF_NOEXPAND};
	let (k, v) = (wide(key), wide(value));
	let mut buf = vec![0u16; 1024];
	let mut len = (buf.len() * 2) as u32;
	let r = unsafe {
		RegGetValueW(
			HKEY_LOCAL_MACHINE,
			k.as_ptr(),
			v.as_ptr(),
			RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
			std::ptr::null_mut(),
			buf.as_mut_ptr() as *mut c_void,
			&mut len,
		)
	};
	if r != 0 {
		return None;
	}
	let n = (len as usize / 2).min(buf.len());
	let s = String::from_utf16_lossy(&buf[..n]);
	Some(s.trim_end_matches('\0').to_string())
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
	use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
	let (k, v) = (wide(key), wide(value));
	let mut out = 0u32;
	let mut len = 4u32;
	let r = unsafe {
		RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut out as *mut u32 as *mut c_void, &mut len)
	};
	(r == 0).then_some(out)
}

/// Duo's install dir: `PULSAR_DUO_DIR`, else the Duo service's image path, else the default.
fn duo_dir() -> Option<std::path::PathBuf> {
	if let Ok(d) = std::env::var("PULSAR_DUO_DIR") {
		return Some(d.into());
	}
	if let Some(img) = reg_string(r"SYSTEM\CurrentControlSet\Services\DuoService", "ImagePath") {
		let exe = img.trim().trim_start_matches('"');
		let exe = exe.split('"').next().unwrap_or(exe);
		if let Some(dir) = std::path::Path::new(exe).parent() {
			if dir.join("DuoController").is_dir() {
				return Some(dir.to_path_buf());
			}
		}
	}
	let def = std::path::PathBuf::from(r"C:\Program Files\Duo");
	def.join("DuoController").is_dir().then_some(def)
}

/// True when this process runs in an interactive session that is NOT the physical console
/// (a Duo/RDP seat). Uses the WinStation name, not `RtlGetActiveConsoleId` — Duo's shim makes
/// the latter report every session as the console.
fn in_remote_session() -> bool {
	use windows_sys::Win32::System::RemoteDesktop::{
		ProcessIdToSessionId, WTSFreeMemory, WTSQuerySessionInformationW, WTSWinStationName, WTS_CURRENT_SERVER_HANDLE,
	};
	let mut sid = 0u32;
	if unsafe { ProcessIdToSessionId(std::process::id(), &mut sid) } == 0 {
		return false;
	}
	let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
	let mut n = 0u32;
	if unsafe { WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, sid, WTSWinStationName, &mut p, &mut n) } == 0 || p.is_null() {
		return false;
	}
	let len = (n as usize / 2).saturating_sub(1);
	let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
	unsafe { WTSFreeMemory(p as *mut c_void) };
	let name = name.trim_end_matches('\0');
	!name.eq_ignore_ascii_case("Console") && !name.is_empty()
}

/// Whether new pads should go through Duo. `PULSAR_PAD_BACKEND=duo|vigem` forces it; otherwise
/// only inside a non-console session of a machine with Duo installed and its HID isolation on
/// (in the console session Duo's shim DENIES `DUOCONTROLLER` devices, so ViGEm stays right there).
pub(crate) fn wanted() -> bool {
	match std::env::var("PULSAR_PAD_BACKEND").ok().as_deref() {
		Some("duo") => return true,
		Some("vigem") => return false,
		_ => {}
	}
	if duo_dir().is_none() || !in_remote_session() {
		return false;
	}
	reg_dword(r"SOFTWARE\Duo", "HostHidIsolation").unwrap_or(1) != 0
}

fn api() -> Option<&'static Api> {
	API.get_or_init(|| unsafe {
		use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
		let dir = duo_dir()?;
		let dll = dir.join("DuoController").join("DuoController.dll");
		let w: Vec<u16> = dll.as_os_str().encode_wide_nul();
		let h = LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
		if h.is_null() {
			tracing::warn!(path = %dll.display(), err = std::io::Error::last_os_error().to_string(), "duo pad: DuoController.dll failed to load");
			return None;
		}
		let sym = |n: &[u8]| GetProcAddress(h, n.as_ptr());
		let (Some(init), Some(create), Some(send), Some(remove)) = (
			sym(b"DuoController_Initialize\0"),
			sym(b"DuoController_CreateController\0"),
			sym(b"DuoController_SendReport\0"),
			sym(b"DuoController_RemoveController\0"),
		) else {
			tracing::warn!("duo pad: DuoController.dll is missing an export (unsupported Duo version)");
			return None;
		};
		let init: FnVoid = std::mem::transmute(init);
		let hr = {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			init()
		};
		if hr < 0 {
			tracing::warn!(hr = format!("{hr:#010x}"), "duo pad: DuoController_Initialize failed");
			return None;
		}
		tracing::info!(path = %dll.display(), "duo pad: DuoController ready (pads are created as this Duo session's own controllers)");
		Some(Api { create: std::mem::transmute(create), send: std::mem::transmute(send), remove: std::mem::transmute(remove) })
	})
	.as_ref()
}

trait EncodeWideNul {
	fn encode_wide_nul(&self) -> Vec<u16>;
}
impl EncodeWideNul for std::ffi::OsStr {
	fn encode_wide_nul(&self) -> Vec<u16> {
		use std::os::windows::ffi::OsStrExt;
		self.encode_wide().chain(std::iter::once(0)).collect()
	}
}

/// Duo's "Xbox One" report (33 bytes).
pub(crate) fn xbox_report(s: &GamepadState) -> [u8; 33] {
	let p = |b: u32| s.is_pressed(b);
	let (up, down, left, right) = (p(button::DPAD_UP), p(button::DPAD_DOWN), p(button::DPAD_LEFT), p(button::DPAD_RIGHT));
	// 0 = none, 1 = N, then clockwise to 8 = NW.
	let hat: u16 = match (up, down, left, right) {
		(true, false, false, false) => 1,
		(true, false, false, true) => 2,
		(false, false, false, true) => 3,
		(false, true, false, true) => 4,
		(false, true, false, false) => 5,
		(false, true, true, false) => 6,
		(false, false, true, false) => 7,
		(true, false, true, false) => 8,
		_ => 0,
	};
	let mut b: u16 = hat << 8;
	for (bit, on) in [
		(1, p(button::GUIDE)),
		(2, p(button::START)),
		(3, p(button::BACK)),
		(4, p(button::A)),
		(5, p(button::B)),
		(6, p(button::X)),
		(7, p(button::Y)),
		(12, p(button::LB)),
		(13, p(button::RB)),
		(14, p(button::L3)),
		(15, p(button::R3)),
	] {
		if on {
			b |= 1 << bit;
		}
	}
	// Unsigned, centre 0x8000; Y inverted the way Duo's Sunshine does it (`~(y + 0x8000)`).
	let x = |v: i16| (v as i32 + 0x8000) as u16;
	let y = |v: i16| !((v as i32 + 0x8000) as u16);
	let mut r = [0u8; 33];
	r[0..2].copy_from_slice(&b.to_le_bytes());
	r[2] = s.left_trigger;
	r[3] = s.right_trigger;
	r[4..6].copy_from_slice(&x(s.left_x).to_le_bytes());
	r[6..8].copy_from_slice(&y(s.left_y).to_le_bytes());
	r[8..10].copy_from_slice(&x(s.right_x).to_le_bytes());
	r[10..12].copy_from_slice(&y(s.right_y).to_le_bytes());
	r
}

/// Duo's DualShock 4 report (30 bytes).
pub(crate) fn ds4_report(s: &GamepadState) -> [u8; 30] {
	// Reuse the ViGEm DS4 field builder for the hat + sticks (Y already inverted there).
	let (vb, _special, lx, ly, rx, ry) = ds4_report_fields(s);
	let hat = (vb & 0x0f) as u8;
	let p = |b: u32| s.is_pressed(b);
	let mut b: u16 = 0;
	for (bit, on) in [
		(0, p(button::X)),     // Square
		(1, p(button::A)),     // Cross
		(2, p(button::B)),     // Circle
		(3, p(button::Y)),     // Triangle
		(4, p(button::LB)),    // L1
		(5, p(button::RB)),    // R1
		(6, s.left_trigger > 0),
		(7, s.right_trigger > 0),
		(8, p(button::BACK)),  // Share
		(9, p(button::START)), // Options
		(10, p(button::L3)),
		(11, p(button::R3)),
		(12, p(button::GUIDE)), // PS
	] {
		if on {
			b |= 1 << bit;
		}
	}
	let mut r = [0u8; 30];
	r[0] = lx;
	r[1] = ly;
	r[2] = rx;
	r[3] = ry;
	r[4] = s.left_trigger;
	r[5] = s.right_trigger;
	r[6] = hat;
	r[7..9].copy_from_slice(&b.to_le_bytes());
	// gyro[3] + accel[3] (+9..+0x15) stay 0. Touch points: bit 7 = NOT touching.
	r[0x15] = 0x80;
	r[0x19] = 0x80;
	r
}

pub struct DuoGamepad {
	kind: GamepadKind,
	xbox: bool,
	handle: *mut c_void,
	rumble_rx: Mutex<Option<mpsc::Receiver<(u8, u8)>>>,
}

// The handle is an opaque DLL object pointer; every use is serialized by `CALL`.
unsafe impl Send for DuoGamepad {}

impl DuoGamepad {
	pub fn new_target(kind: GamepadKind, resolved: ResolvedTarget) -> Result<Self, Box<dyn std::error::Error>> {
		let api = api().ok_or("DuoController unavailable")?;
		let (ty, xbox) = match resolved {
			ResolvedTarget::Xbox360 => (0u32, true),
			ResolvedTarget::Ds4 => (1u32, false),
		};
		let mut h: *mut c_void = std::ptr::null_mut();
		let hr = {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			unsafe { (api.create)(ty, Some(on_feedback), std::ptr::null_mut(), &mut h) }
		};
		if hr < 0 || h.is_null() {
			return Err(format!("DuoController_CreateController failed hr={hr:#010x}").into());
		}
		let (tx, rx) = mpsc::sync_channel(16);
		rumble_map().lock().unwrap_or_else(|e| e.into_inner()).insert(h as usize, tx);
		tracing::info!(kind = ?kind, ty, "duo pad: created");
		Ok(Self { kind, xbox, handle: h, rumble_rx: Mutex::new(Some(rx)) })
	}
}

impl VirtualGamepad for DuoGamepad {
	fn kind(&self) -> GamepadKind {
		self.kind
	}

	fn apply(&mut self, state: &GamepadState) {
		let Some(api) = api() else { return };
		let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
		let hr = if self.xbox {
			let mut r = xbox_report(state);
			unsafe { (api.send)(self.handle, r.as_mut_ptr()) }
		} else {
			let mut r = ds4_report(state);
			unsafe { (api.send)(self.handle, r.as_mut_ptr()) }
		};
		if hr < 0 {
			use std::sync::Once;
			static WARN: Once = Once::new();
			WARN.call_once(|| tracing::warn!(hr = format!("{hr:#010x}"), "duo pad: SendReport failed"));
		}
	}

	fn rumble_reader(&self) -> Option<Box<dyn RumbleReader>> {
		let rx = self.rumble_rx.lock().unwrap_or_else(|e| e.into_inner()).take()?;
		Some(Box::new(DuoRumble { rx }))
	}
}

impl Drop for DuoGamepad {
	fn drop(&mut self) {
		rumble_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&(self.handle as usize));
		if let Some(api) = api() {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			let hr = unsafe { (api.remove)(self.handle) };
			tracing::info!(hr = format!("{hr:#010x}"), "duo pad: removed");
		}
	}
}

struct DuoRumble {
	rx: mpsc::Receiver<(u8, u8)>,
}
impl RumbleReader for DuoRumble {
	fn next(&mut self) -> Option<(u8, u8)> {
		// Ends (None) when the pad is dropped: its sender leaves the map.
		self.rx.recv().ok()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn xbox_report_layout() {
		let mut s = GamepadState::default();
		let r = xbox_report(&s);
		assert_eq!(u16::from_le_bytes([r[0], r[1]]), 0);
		assert_eq!(u16::from_le_bytes([r[4], r[5]]), 0x8000); // centred X
		assert_eq!(u16::from_le_bytes([r[6], r[7]]), 0x7fff); // centred Y (inverted)
		s.set(button::A, true);
		s.set(button::GUIDE, true);
		s.set(button::DPAD_UP, true);
		s.left_y = i16::MAX; // stick fully up
		s.right_trigger = 200;
		let r = xbox_report(&s);
		let b = u16::from_le_bytes([r[0], r[1]]);
		assert_eq!(b & (1 << 4), 1 << 4); // A
		assert_eq!(b & (1 << 1), 1 << 1); // Guide
		assert_eq!((b >> 8) & 0xf, 1); // hat N
		assert_eq!(r[3], 200);
		assert_eq!(u16::from_le_bytes([r[6], r[7]]), 0); // up = 0 after inversion
	}

	#[test]
	fn ds4_report_layout() {
		let mut s = GamepadState::default();
		let r = ds4_report(&s);
		assert_eq!(r[6], 8); // hat neutral
		assert_eq!([r[0], r[1]], [0x80, 0x80]);
		assert_eq!(r[0x15] & 0x80, 0x80);
		s.set(button::X, true); // Square
		s.set(button::GUIDE, true); // PS
		s.set(button::DPAD_RIGHT, true);
		s.left_trigger = 10;
		let r = ds4_report(&s);
		let b = u16::from_le_bytes([r[7], r[8]]);
		assert_eq!(b & 1, 1);
		assert_eq!(b & (1 << 12), 1 << 12);
		assert_eq!(b & (1 << 6), 1 << 6);
		assert_eq!(r[6], 2); // hat E
		assert_eq!(r[4], 10);
	}
}
