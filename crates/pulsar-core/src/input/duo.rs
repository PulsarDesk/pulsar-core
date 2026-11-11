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
//!   DualSense/Edge take the raw 63-byte USB input state (report 0x01 minus its id), which the
//!   DLL rewrites in place (seq, sensor timestamps, default battery) — see [`DuoDs5`].
//! - `DuoController_RemoveController(handle)`.
//! - Feedback: `cb(handle, const u8 fb[8], ctx)` on a DLL thread; fb[3] = strong, fb[4] = weak.
//!
//! The DLL keeps a global handle array without a lock, so every call goes through one mutex.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use super::ds5::duo::{
	duo_wanted, event_name, is_duo_ds5_hid_path, parse_output, pick_new_parent, rumble_state,
	section_name, SECTION_PREFIX,
};
use super::ds5::virtual_pad::Ds5HostEvent;
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
	let force = std::env::var("PULSAR_PAD_BACKEND").ok();
	match force.as_deref() {
		Some("duo") | Some("vigem") => duo_wanted(force.as_deref(), false, false, None),
		_ => duo_wanted(
			None,
			duo_dir().is_some(),
			in_remote_session(),
			reg_dword(r"SOFTWARE\Duo", "HostHidIsolation"),
		),
	}
}

/// Public form of [`wanted`]: pads (incl. DualSense) should be Duo controllers here.
pub fn duo_pads_wanted() -> bool {
	wanted()
}

/// `DuoController.dll` is installed, exports the expected API and initialized.
pub fn duo_available() -> bool {
	api().is_some()
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

/// Wait this long for a new Duo DualSense's devnode + output section after
/// `CreateController` (the device appears ~1.5 s later).
const DS5_DISCOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// The output-section reader re-reads at least this often even without an event (the
/// event may be auto-reset and consumed by the DLL's own feedback thread).
const DS5_POLL: u32 = 8;
/// Serializes DS5 create + devnode discovery, so two pads never pick each other's devnode.
static DS5_DISCOVER: Mutex<()> = Mutex::new(());
/// Parent devnode ids owned by live [`DuoDs5`] pads.
static DS5_CLAIMED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A virtual DualSense (or Edge) created through Duo's driver, for the DualSense
/// passthrough host inside a Duo seat. HID only: input state in, the game's output state
/// (triggers, LEDs, rumble) out as [`Ds5HostEvent::Output`]; no haptics, speaker or mic
/// (Duo's DualSense has no audio interface).
///
/// Output comes from Duo's shared output section (see `ds5::duo`); if that can't be found,
/// from the DLL's rumble callback only. Dropping it removes the device.
pub struct DuoDs5 {
	handle: *mut c_void,
	parent: Option<String>,
	stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
	reader: Option<std::thread::JoinHandle<()>>,
}

// Opaque DLL object pointer; every DLL call is serialized by `CALL`.
unsafe impl Send for DuoDs5 {}
unsafe impl Sync for DuoDs5 {}

impl DuoDs5 {
	/// Create the controller (type 2, or 3 for `edge`) and start the output reader. Blocks
	/// up to ~5 s while the device appears.
	pub fn start(edge: bool) -> std::io::Result<(Self, mpsc::Receiver<Ds5HostEvent>)> {
		use std::io::{Error, ErrorKind};
		let api =
			api().ok_or_else(|| Error::new(ErrorKind::NotFound, "DuoController unavailable"))?;
		let ty = if edge { 3u32 } else { 2u32 };
		let _d = DS5_DISCOVER.lock().unwrap_or_else(|e| e.into_inner());
		let before = duo_ds5_parents(edge);
		let mut h: *mut c_void = std::ptr::null_mut();
		let hr = {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			unsafe { (api.create)(ty, Some(on_feedback), std::ptr::null_mut(), &mut h) }
		};
		if hr < 0 || h.is_null() {
			return Err(Error::other(format!(
				"DuoController_CreateController({ty}) failed hr={hr:#010x}"
			)));
		}
		let (rumble_tx, rumble_rx) = mpsc::sync_channel(16);
		rumble_map()
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.insert(h as usize, rumble_tx);
		let mut pad = Self {
			handle: h,
			parent: None,
			stop: Default::default(),
			reader: None,
		};
		// No report yet: SendReport waits up to 1 s for the device's input section and
		// fails until it exists; the caller's first state follows right after `start`.
		let section = Ds5Section::discover(&before, edge);
		if let Some(s) = &section {
			DS5_CLAIMED
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.push(s.parent.clone());
			pad.parent = Some(s.parent.clone());
			tracing::info!(parent = %s.parent, "duo ds5: created; reading the game's output report from Duo's section");
		} else {
			tracing::warn!("duo ds5: created, but its output section wasn't found; rumble only (no triggers/LEDs)");
		}
		drop(_d);
		let (tx, rx) = mpsc::channel();
		let stop = pad.stop.clone();
		pad.reader = std::thread::Builder::new()
			.name("duo-ds5-out".into())
			.spawn(move || ds5_reader(section, rumble_rx, tx, stop))
			.ok();
		Ok((pad, rx))
	}

	/// Send one input state: the raw USB input report 0x01 without its id byte. The DLL
	/// rewrites the sequence byte, both sensor timestamps and zero battery bytes in place.
	pub fn send_raw(&self, state: &mut [u8; super::ds5::report::STATE_LEN]) -> bool {
		let Some(api) = api() else { return false };
		let hr = {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			unsafe { (api.send)(self.handle, state.as_mut_ptr()) }
		};
		if hr < 0 {
			use std::sync::Once;
			static WARN: Once = Once::new();
			WARN.call_once(|| {
				tracing::warn!(hr = format!("{hr:#010x}"), "duo ds5: SendReport failed")
			});
		}
		hr >= 0
	}

	/// The controller's ROOT devnode instance id, if found.
	pub fn parent_instance_id(&self) -> Option<&str> {
		self.parent.as_deref()
	}
}

impl Drop for DuoDs5 {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
		// Ends the reader's rumble channel too.
		rumble_map()
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.remove(&(self.handle as usize));
		if let Some(t) = self.reader.take() {
			let _ = t.join();
		}
		if let Some(api) = api() {
			let _g = CALL.lock().unwrap_or_else(|e| e.into_inner());
			let hr = unsafe { (api.remove)(self.handle) };
			tracing::info!(hr = format!("{hr:#010x}"), "duo ds5: removed");
		}
		if let Some(p) = &self.parent {
			DS5_CLAIMED
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.retain(|c| c != p);
		}
	}
}

/// Duo's mapped output section for one controller.
struct Ds5Section {
	parent: String,
	map: windows_sys::Win32::Foundation::HANDLE,
	view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
	/// `<section>_event`, or null (then the reader only polls).
	event: windows_sys::Win32::Foundation::HANDLE,
}

// Plain kernel handles + a read-only view; used by one thread at a time.
unsafe impl Send for Ds5Section {}

impl Ds5Section {
	/// Find the new controller's devnode (not in `before`, not claimed) and open its
	/// section, retrying until [`DS5_DISCOVER_TIMEOUT`].
	fn discover(before: &[String], edge: bool) -> Option<Self> {
		let deadline = std::time::Instant::now() + DS5_DISCOVER_TIMEOUT;
		let mut parent: Option<String> = None;
		loop {
			if parent.is_none() {
				let claimed = DS5_CLAIMED
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.clone();
				parent = pick_new_parent(before, &duo_ds5_parents(edge), &claimed);
			}
			if let Some(p) = &parent {
				if let Some(s) = Self::open(p) {
					return Some(s);
				}
			}
			if std::time::Instant::now() >= deadline {
				tracing::warn!(parent = ?parent, "duo ds5: no output section within {DS5_DISCOVER_TIMEOUT:?}");
				return None;
			}
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
	}

	fn open(parent: &str) -> Option<Self> {
		use windows_sys::Win32::Foundation::CloseHandle;
		use windows_sys::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ};
		use windows_sys::Win32::System::Threading::OpenEventW;
		const SYNCHRONIZE: u32 = 0x0010_0000;
		let name = wide(&section_name(parent));
		let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
		if map.is_null() {
			return None;
		}
		let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, SECTION_PREFIX) };
		if view.Value.is_null() {
			unsafe { CloseHandle(map) };
			return None;
		}
		let ev = wide(&event_name(parent));
		let event = unsafe { OpenEventW(SYNCHRONIZE, 0, ev.as_ptr()) };
		if event.is_null() {
			tracing::debug!("duo ds5: output event not openable; polling the section");
		}
		Some(Self {
			parent: parent.to_string(),
			map,
			view,
			event,
		})
	}

	/// A consistent copy of the section's first bytes (re-read until two reads agree, so
	/// a write in progress isn't taken half-done).
	fn snapshot(&self) -> [u8; SECTION_PREFIX] {
		let p = self.view.Value as *const u8;
		let read = || {
			let mut b = [0u8; SECTION_PREFIX];
			for (i, x) in b.iter_mut().enumerate() {
				*x = unsafe { std::ptr::read_volatile(p.add(i)) };
			}
			b
		};
		let mut a = read();
		for _ in 0..4 {
			let b = read();
			if a == b {
				break;
			}
			a = b;
		}
		a
	}

	/// Wait for the driver's event, at most [`DS5_POLL`] ms.
	fn wait(&self) {
		use windows_sys::Win32::System::Threading::WaitForSingleObject;
		if self.event.is_null() {
			std::thread::sleep(std::time::Duration::from_millis(DS5_POLL as u64));
		} else {
			unsafe { WaitForSingleObject(self.event, DS5_POLL) };
		}
	}
}

impl Drop for Ds5Section {
	fn drop(&mut self) {
		use windows_sys::Win32::Foundation::CloseHandle;
		use windows_sys::Win32::System::Memory::UnmapViewOfFile;
		unsafe {
			UnmapViewOfFile(self.view);
			CloseHandle(self.map);
			if !self.event.is_null() {
				CloseHandle(self.event);
			}
		}
	}
}

/// Turns the output section (or, without it, the DLL's rumble callback) into
/// `Ds5HostEvent::Output` on change. Ends on `stop` or when the receiver is gone.
fn ds5_reader(
	section: Option<Ds5Section>,
	rumble: mpsc::Receiver<(u8, u8)>,
	tx: mpsc::Sender<Ds5HostEvent>,
	stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
	use std::sync::atomic::Ordering;
	let mut last: Option<[u8; SECTION_PREFIX]> = None;
	while !stop.load(Ordering::Relaxed) {
		match &section {
			Some(s) => {
				s.wait();
				// The section carries the rumble too; the callback's copy would double it.
				while rumble.try_recv().is_ok() {}
				let snap = s.snapshot();
				if last == Some(snap) {
					continue;
				}
				last = Some(snap);
				if let Some(state) = parse_output(&snap) {
					if tx.send(Ds5HostEvent::Output(state)).is_err() {
						return;
					}
				}
			}
			None => match rumble.recv_timeout(std::time::Duration::from_millis(50)) {
				Ok((strong, weak)) => {
					if tx
						.send(Ds5HostEvent::Output(rumble_state(strong, weak)))
						.is_err()
					{
						return;
					}
				}
				Err(mpsc::RecvTimeoutError::Timeout) => {}
				Err(mpsc::RecvTimeoutError::Disconnected) => return,
			},
		}
	}
}

/// Parent (ROOT) devnode ids of every present Duo DualSense HID interface.
fn duo_ds5_parents(edge: bool) -> Vec<String> {
	duo_ds5_interfaces(edge).into_iter().map(|(_, parent)| parent).collect()
}

/// Every present Duo DualSense (`edge`: Edge) HID interface as `(device path, parent ROOT
/// devnode instance id)`. For tools and tests that talk to the pad's HID side directly.
pub fn duo_ds5_interfaces(edge: bool) -> Vec<(String, String)> {
	use windows_sys::core::GUID;
	use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
	// GUID_DEVINTERFACE_HID.
	const HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);
	let mut out = Vec::new();
	unsafe {
		let set = SetupDiGetClassDevsW(
			&HID,
			std::ptr::null(),
			std::ptr::null_mut(),
			DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
		);
		if set as isize == -1 || set as isize == 0 {
			return out;
		}
		let mut i = 0;
		loop {
			let mut ifd: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
			ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
			if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &HID, i, &mut ifd) == 0 {
				break;
			}
			i += 1;
			let mut need = 0u32;
			SetupDiGetDeviceInterfaceDetailW(
				set,
				&ifd,
				std::ptr::null_mut(),
				0,
				&mut need,
				std::ptr::null_mut(),
			);
			if need == 0 {
				continue;
			}
			// u32-aligned buffer for SP_DEVICE_INTERFACE_DETAIL_DATA_W.
			let mut buf = vec![0u32; (need as usize).div_ceil(4) + 2];
			let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
			(*det).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
			let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
			dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
			if SetupDiGetDeviceInterfaceDetailW(
				set,
				&ifd,
				det,
				need,
				std::ptr::null_mut(),
				&mut dev,
			) == 0
			{
				continue;
			}
			let p = (*det).DevicePath.as_ptr();
			let mut n = 0;
			while *p.add(n) != 0 {
				n += 1;
			}
			let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
			if !is_duo_ds5_hid_path(&path, edge) {
				continue;
			}
			let mut parent = 0u32;
			if CM_Get_Parent(&mut parent, dev.DevInst, 0) != CR_SUCCESS {
				continue;
			}
			let mut id = [0u16; 512];
			if CM_Get_Device_IDW(parent, id.as_mut_ptr(), id.len() as u32, 0) == CR_SUCCESS {
				let n = id.iter().position(|c| *c == 0).unwrap_or(id.len());
				out.push((path, String::from_utf16_lossy(&id[..n])));
			}
		}
		SetupDiDestroyDeviceInfoList(set);
	}
	out
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
