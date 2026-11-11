//! Removes virtual pads a dead Pulsar left behind, and records the pads this process
//! plugs in so a later Pulsar can do the same for it. The decisions are pure and tested in
//! [`super::pad_registry`]; this file is the Windows I/O (SetupAPI/CfgMgr32, process probe,
//! the record file). Other platforms: everything is a no-op (Linux vhci-hcd unplugs a pad by
//! itself when its server socket dies, and there is no Duo there).
//!
//! The record file is machine-wide, `%ProgramData%\Pulsar\duo-pads.json` (all pad kinds,
//! the name is historical), owned by Administrators with a SYSTEM + Administrators only DACL.
//! Machine-wide because a Duo host has one Pulsar per seat, each in another user's session:
//! a per-user file could only clean up after the same user's Pulsar, and the next Pulsar
//! started in that seat may run as anyone. Every Pulsar that creates Duo or USB/IP pads is
//! elevated anyway (both drivers require it), so an admin-only file costs nothing. Not
//! elevated: nothing is recorded or removed (logged once).
//!
//! Entry points: [`startup`] (host start), [`before_create`] (throttled; the pad backends
//! call it before plugging a pad in) and [`PadLease`] (one per live pad; drop = unrecord).

#[cfg(windows)]
pub use win::{before_create, duo_root_ids, startup, PadLease};

#[cfg(not(windows))]
mod stub {
	/// No-op off Windows.
	pub fn startup() {}
	/// No-op off Windows.
	pub fn before_create() {}
	/// No-op off Windows.
	pub fn duo_root_ids() -> Vec<String> {
		Vec::new()
	}
	/// No-op off Windows.
	pub struct PadLease;
	impl PadLease {
		pub fn none() -> Self {
			Self
		}
		pub fn usbip(_tcp_port: u16, _vhci_port: Option<u32>) -> Self {
			Self
		}
	}
}
#[cfg(not(windows))]
pub use stub::{before_create, duo_root_ids, startup, PadLease};

#[cfg(windows)]
mod win {
	use std::io::{Read, Seek, Write};
	use std::os::windows::fs::OpenOptionsExt;
	use std::path::{Path, PathBuf};
	use std::sync::{Arc, Mutex, OnceLock};
	use std::time::{Duration, Instant};

	use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
	use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, FILETIME, HANDLE};
	use windows_sys::Win32::Security::Authorization::{
		ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
		SDDL_REVISION_1, SE_FILE_OBJECT,
	};
	use windows_sys::Win32::Security::{
		GetTokenInformation, IsWellKnownSid, TokenElevation, WinBuiltinAdministratorsSid,
		WinLocalSystemSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
		SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY,
	};
	use windows_sys::Win32::Storage::FileSystem::{
		CreateDirectoryW, CreateFileW, GetFileAttributesW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
		FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_WRITE, INVALID_FILE_ATTRIBUTES,
	};
	use windows_sys::Win32::System::Threading::{
		GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
		PROCESS_QUERY_LIMITED_INFORMATION,
	};

	use super::super::ds5::attach;
	use super::super::pad_registry::{
		hid_is_orphan, is_duo_hid_instance, is_duo_root_instance, pick_new_root, plan_reap,
		usbip_reap, Owner, PadRecord, ParentState, ProcState, Registry,
	};

	/// `%ProgramData%\Pulsar` when Pulsar creates it here (same DACL the desktop's shared
	/// mode uses: SYSTEM + Administrators full, Users read).
	const SDDL_DIR: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
	/// The record file: SYSTEM + Administrators only.
	const SDDL_FILE: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)";
	const FILE_NAME: &str = "duo-pads.json";
	/// How long to wait for another Pulsar holding the record file.
	const LOCK_WAIT: Duration = Duration::from_secs(2);
	/// [`before_create`] sweeps at most this often.
	const SWEEP_EVERY: Duration = Duration::from_secs(30);
	/// A new Duo controller's ROOT devnode shows up within this long (~1.5 s seen).
	const DISCOVER_TIMEOUT: Duration = Duration::from_secs(6);
	const STILL_ACTIVE: u32 = 259;

	fn wide(s: &str) -> Vec<u16> {
		s.encode_utf16().chain(std::iter::once(0)).collect()
	}

	fn wide_path(p: &Path) -> Vec<u16> {
		use std::os::windows::ffi::OsStrExt;
		p.as_os_str()
			.encode_wide()
			.chain(std::iter::once(0))
			.collect()
	}

	fn from_wide(buf: &[u16]) -> String {
		let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
		String::from_utf16_lossy(&buf[..n])
	}

	/// Whether this process is elevated (cached). Removing devices and the admin-only record
	/// file both need it.
	fn elevated() -> bool {
		static E: OnceLock<bool> = OnceLock::new();
		*E.get_or_init(|| unsafe {
			let mut token: HANDLE = std::ptr::null_mut();
			if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
				return false;
			}
			let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
			let mut len = 0u32;
			let ok = GetTokenInformation(
				token,
				TokenElevation,
				&mut elev as *mut _ as *mut _,
				std::mem::size_of::<TOKEN_ELEVATION>() as u32,
				&mut len,
			);
			CloseHandle(token);
			let e = ok != 0 && elev.TokenIsElevated != 0;
			if !e {
				tracing::info!("pad cleanup: not elevated; leftover virtual pads are neither recorded nor removed");
			}
			e
		})
	}

	// ---- processes ----

	fn filetime(f: FILETIME) -> u64 {
		(f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
	}

	fn proc_state(pid: u32) -> ProcState {
		unsafe {
			let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
			if h.is_null() {
				// ERROR_INVALID_PARAMETER = no such pid. Anything else (access denied):
				// it exists; don't guess.
				let e = std::io::Error::last_os_error().raw_os_error();
				return if e == Some(87) {
					ProcState::Gone
				} else {
					ProcState::Running(None)
				};
			}
			let mut code = 0u32;
			let exited = GetExitCodeProcess(h, &mut code) != 0 && code != STILL_ACTIVE;
			let z = FILETIME {
				dwLowDateTime: 0,
				dwHighDateTime: 0,
			};
			let (mut c, mut e, mut k, mut u) = (z, z, z, z);
			let started =
				(GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0).then(|| filetime(c));
			CloseHandle(h);
			if exited {
				ProcState::Gone
			} else {
				ProcState::Running(started)
			}
		}
	}

	fn me() -> Owner {
		static ME: OnceLock<Owner> = OnceLock::new();
		*ME.get_or_init(|| {
			let pid = std::process::id();
			let started = match proc_state(pid) {
				ProcState::Running(Some(t)) => t,
				_ => 0,
			};
			Owner { pid, started }
		})
	}

	// ---- the record file ----

	struct LocalSd(PSECURITY_DESCRIPTOR);
	impl Drop for LocalSd {
		fn drop(&mut self) {
			unsafe { LocalFree(self.0 as _) };
		}
	}

	fn sd(sddl: &str) -> std::io::Result<LocalSd> {
		let w = wide(sddl);
		let mut p: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
		if unsafe {
			ConvertStringSecurityDescriptorToSecurityDescriptorW(
				w.as_ptr(),
				SDDL_REVISION_1,
				&mut p,
				std::ptr::null_mut(),
			)
		} == 0
		{
			return Err(std::io::Error::last_os_error());
		}
		Ok(LocalSd(p))
	}

	/// The path exists, is not a link, and is owned by SYSTEM or Administrators (a standard
	/// user could have pre-created it to plant records).
	fn check_trusted(w: &[u16], what: &Path) -> std::io::Result<()> {
		let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
		if attrs == INVALID_FILE_ATTRIBUTES {
			return Err(std::io::Error::last_os_error());
		}
		if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
			return Err(std::io::Error::other(format!(
				"{} is a link",
				what.display()
			)));
		}
		let mut owner: PSID = std::ptr::null_mut();
		let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
		unsafe {
			let rc = GetNamedSecurityInfoW(
				w.as_ptr(),
				SE_FILE_OBJECT,
				OWNER_SECURITY_INFORMATION,
				&mut owner,
				std::ptr::null_mut(),
				std::ptr::null_mut(),
				std::ptr::null_mut(),
				&mut psd,
			);
			if rc != 0 {
				return Err(std::io::Error::from_raw_os_error(rc as i32));
			}
			let _g = LocalSd(psd);
			if IsWellKnownSid(owner, WinBuiltinAdministratorsSid) == 0
				&& IsWellKnownSid(owner, WinLocalSystemSid) == 0
			{
				return Err(std::io::Error::other(format!(
					"{} is not owned by SYSTEM/Administrators",
					what.display()
				)));
			}
		}
		Ok(())
	}

	fn machine_dir() -> Option<PathBuf> {
		use windows_sys::Win32::System::Com::CoTaskMemFree;
		use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
		let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
		let hr =
			unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut p) };
		if hr < 0 || p.is_null() {
			if !p.is_null() {
				unsafe { CoTaskMemFree(p as _) };
			}
			return None;
		}
		let s = unsafe {
			let mut n = 0;
			while *p.add(n) != 0 {
				n += 1;
			}
			String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
		};
		unsafe { CoTaskMemFree(p as _) };
		Some(PathBuf::from(s).join("Pulsar"))
	}

	/// Create (with its DACL) or validate the record file; returns its path.
	fn record_path() -> std::io::Result<PathBuf> {
		let dir = machine_dir().ok_or_else(|| std::io::Error::other("no ProgramData"))?;
		let wd = wide_path(&dir);
		let dsd = sd(SDDL_DIR)?;
		let sa = SECURITY_ATTRIBUTES {
			nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
			lpSecurityDescriptor: dsd.0,
			bInheritHandle: 0,
		};
		if unsafe { CreateDirectoryW(wd.as_ptr(), &sa) } == 0 {
			check_trusted(&wd, &dir)?;
		}
		let file = dir.join(FILE_NAME);
		let wf = wide_path(&file);
		let fsd = sd(SDDL_FILE)?;
		let sa = SECURITY_ATTRIBUTES {
			nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
			lpSecurityDescriptor: fsd.0,
			bInheritHandle: 0,
		};
		let h = unsafe {
			CreateFileW(
				wf.as_ptr(),
				FILE_GENERIC_WRITE,
				0,
				&sa,
				CREATE_NEW,
				FILE_ATTRIBUTE_NORMAL,
				std::ptr::null_mut(),
			)
		};
		if h as isize == -1 {
			check_trusted(&wf, &file)?;
		} else {
			unsafe { CloseHandle(h) };
		}
		Ok(file)
	}

	/// Run `f` on the record file under an exclusive open (other Pulsars wait up to
	/// [`LOCK_WAIT`]); written back when `f` changed it. `None` when not elevated or the
	/// file can't be used (logged once).
	fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> Option<R> {
		if !elevated() {
			return None;
		}
		let path = match record_path() {
			Ok(p) => p,
			Err(e) => {
				static WARN: std::sync::Once = std::sync::Once::new();
				WARN.call_once(|| tracing::warn!("pad cleanup: record file unusable: {e}"));
				return None;
			}
		};
		let deadline = Instant::now() + LOCK_WAIT;
		let mut file = loop {
			match std::fs::OpenOptions::new()
				.read(true)
				.write(true)
				.share_mode(0)
				.open(&path)
			{
				Ok(f) => break f,
				// ERROR_SHARING_VIOLATION: another Pulsar has it open right now.
				Err(e) if e.raw_os_error() == Some(32) && Instant::now() < deadline => {
					std::thread::sleep(Duration::from_millis(25));
				}
				Err(e) => {
					tracing::warn!("pad cleanup: {}: {e}", path.display());
					return None;
				}
			}
		};
		let mut raw = String::new();
		let _ = file.read_to_string(&mut raw);
		let mut reg = Registry::parse(&raw);
		let before = reg.clone();
		let out = f(&mut reg);
		if reg != before {
			let json = reg.to_json();
			let res = file
				.seek(std::io::SeekFrom::Start(0))
				.and_then(|_| file.set_len(0))
				.and_then(|_| file.write_all(json.as_bytes()));
			if let Err(e) = res {
				tracing::warn!("pad cleanup: writing {}: {e}", path.display());
			}
		}
		Some(out)
	}

	// ---- devices ----

	/// Every present devnode under `enumerator` (`ROOT`, `HID`) as `(instance id, devinst)`.
	fn devnodes(enumerator: &str) -> Vec<(String, u32)> {
		let mut out = Vec::new();
		let e = wide(enumerator);
		unsafe {
			let set = SetupDiGetClassDevsW(
				std::ptr::null(),
				e.as_ptr(),
				std::ptr::null_mut(),
				DIGCF_ALLCLASSES | DIGCF_PRESENT,
			);
			if set as isize == -1 || set as isize == 0 {
				return out;
			}
			let mut i = 0;
			loop {
				let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
				dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
				if SetupDiEnumDeviceInfo(set, i, &mut dev) == 0 {
					break;
				}
				i += 1;
				let mut id = [0u16; 512];
				if SetupDiGetDeviceInstanceIdW(
					set,
					&dev,
					id.as_mut_ptr(),
					id.len() as u32,
					std::ptr::null_mut(),
				) != 0
				{
					out.push((from_wide(&id), dev.DevInst));
				}
			}
			SetupDiDestroyDeviceInfoList(set);
		}
		out
	}

	/// ROOT devnode ids of every present Duo controller (all seats).
	pub fn duo_root_ids() -> Vec<String> {
		if !elevated() {
			return Vec::new();
		}
		devnodes("ROOT")
			.into_iter()
			.map(|(id, _)| id)
			.filter(|id| is_duo_root_instance(id))
			.collect()
	}

	fn parent_state(devinst: u32) -> ParentState {
		unsafe {
			let mut parent = 0u32;
			if CM_Get_Parent(&mut parent, devinst, 0) != CR_SUCCESS {
				return ParentState::Missing;
			}
			let mut id = [0u16; 512];
			if CM_Get_Device_IDW(parent, id.as_mut_ptr(), id.len() as u32, 0) != CR_SUCCESS {
				return ParentState::Missing;
			}
			let id = from_wide(&id);
			if id.eq_ignore_ascii_case(r"HTREE\ROOT\0") {
				return ParentState::TreeRoot;
			}
			let (mut status, mut problem) = (0u32, 0u32);
			if CM_Get_DevNode_Status(&mut status, &mut problem, parent, 0) != CR_SUCCESS {
				return ParentState::NotPresent;
			}
			if status & DN_HAS_PROBLEM == 0 {
				problem = 0;
			}
			ParentState::Present { id, problem }
		}
	}

	enum Removed {
		Yes {
			reboot: bool,
		},
		/// No such devnode (already gone).
		Gone,
		Failed(std::io::Error),
	}

	/// `DIF_REMOVE` (what Device Manager's "Uninstall device" / `pnputil /remove-device`
	/// do): removes the devnode and its children, present or not.
	fn remove_device(instance_id: &str) -> Removed {
		let w = wide(instance_id);
		unsafe {
			let set = SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut());
			if set as isize == -1 {
				return Removed::Failed(std::io::Error::last_os_error());
			}
			let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
			dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
			if SetupDiOpenDeviceInfoW(set, w.as_ptr(), std::ptr::null_mut(), 0, &mut dev) == 0 {
				let e = std::io::Error::last_os_error();
				SetupDiDestroyDeviceInfoList(set);
				// ERROR_NO_SUCH_DEVINST (0xE000020B).
				return if e.raw_os_error() == Some(0xE000_020Bu32 as i32) {
					Removed::Gone
				} else {
					Removed::Failed(e)
				};
			}
			let params = SP_REMOVEDEVICE_PARAMS {
				ClassInstallHeader: SP_CLASSINSTALL_HEADER {
					cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
					InstallFunction: DIF_REMOVE,
				},
				Scope: DI_REMOVEDEVICE_GLOBAL,
				HwProfile: 0,
			};
			let mut res = if SetupDiSetClassInstallParamsW(
				set,
				&dev,
				&params.ClassInstallHeader,
				std::mem::size_of::<SP_REMOVEDEVICE_PARAMS>() as u32,
			) == 0 || SetupDiCallClassInstaller(DIF_REMOVE, set, &dev) == 0
			{
				Removed::Failed(std::io::Error::last_os_error())
			} else {
				Removed::Yes { reboot: false }
			};
			if let Removed::Yes { reboot } = &mut res {
				let mut ip: SP_DEVINSTALL_PARAMS_W = std::mem::zeroed();
				ip.cbSize = std::mem::size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
				if SetupDiGetDeviceInstallParamsW(set, &dev, &mut ip) != 0 {
					*reboot = ip.Flags & (DI_NEEDREBOOT | DI_NEEDRESTART) != 0;
				}
			}
			SetupDiDestroyDeviceInfoList(set);
			res
		}
	}

	fn log_removed(what: &str, id: &str, r: &Removed) {
		match r {
			Removed::Yes { reboot } => {
				tracing::info!(id, reboot, "pad cleanup: removed {what}")
			}
			Removed::Gone => tracing::debug!(id, "pad cleanup: {what} already gone"),
			Removed::Failed(e) => tracing::warn!(id, "pad cleanup: removing {what} failed: {e}"),
		}
	}

	/// Rule (a): present Duo HID devnodes whose controller devnode is gone, any seat.
	fn sweep_orphans() {
		for (id, devinst) in devnodes("HID") {
			if !is_duo_hid_instance(&id) {
				continue;
			}
			let parent = parent_state(devinst);
			if hid_is_orphan(&id, &parent) {
				tracing::warn!(id, parent = ?parent, "pad cleanup: orphaned Duo HID device (its controller devnode is gone)");
				let r = remove_device(&id);
				log_removed("orphaned Duo HID device", &id, &r);
			}
		}
	}

	/// `(vhci port, server tcp port)` of each 127.0.0.1 import, from `usbip port`.
	fn usbip_imports(exe: &Path) -> Vec<(u32, u16)> {
		match attach::port_listing(exe) {
			Ok(out) => attach::parse_port_targets(&out),
			Err(e) => {
				tracing::debug!("pad cleanup: usbip port: {e}");
				Vec::new()
			}
		}
	}

	/// Rule (b): recorded pads whose creating process is gone.
	fn reap_dead_owners() {
		let Some((reap, live)) = with_registry(|reg| {
			let (keep, reap) = plan_reap(&reg.pads, proc_state);
			// Out of the file before acting: the actions can take seconds and other
			// Pulsars shouldn't wait on the lock. Failures are put back below.
			reg.pads = keep.clone();
			(reap, keep)
		}) else {
			return;
		};
		if reap.is_empty() {
			return;
		}
		let mut retry = Vec::new();
		let mut usbip: Option<(PathBuf, Vec<(u32, u16)>)> = None;
		for rec in reap {
			match &rec {
				PadRecord::Duo { instance_id, owner } => {
					if !is_duo_root_instance(instance_id) {
						tracing::warn!(id = %instance_id, "pad cleanup: ignoring a record that isn't a Duo controller");
						continue;
					}
					tracing::info!(id = %instance_id, pid = owner.pid, "pad cleanup: Duo controller of a Pulsar that is gone");
					let r = remove_device(instance_id);
					log_removed("leftover Duo controller", instance_id, &r);
					if matches!(r, Removed::Failed(_)) {
						retry.push(rec.clone());
					}
				}
				PadRecord::Usbip {
					tcp_port, owner, ..
				} => {
					let Ok(exe) = attach::locate(None) else {
						continue;
					};
					let imports = &usbip
						.get_or_insert_with(|| {
							let i = usbip_imports(&exe);
							(exe.clone(), i)
						})
						.1;
					let plan = usbip_reap(&rec, &live, imports);
					tracing::info!(tcp_port, pid = owner.pid, plan = ?plan, "pad cleanup: USB/IP pad of a Pulsar that is gone");
					if let Some(p) = plan.detach {
						if !attach::detach_port(&exe, p) {
							retry.push(rec.clone());
						}
					}
					if plan.stop_reattach {
						attach::stop_attach_attempts(&exe, *tcp_port);
					}
				}
			}
		}
		if !retry.is_empty() {
			with_registry(|reg| {
				for r in retry {
					reg.add(r);
				}
			});
		}
	}

	fn sweep(reason: &str) {
		if !elevated() {
			return;
		}
		let t = Instant::now();
		sweep_orphans();
		reap_dead_owners();
		tracing::debug!(
			reason,
			ms = t.elapsed().as_millis() as u64,
			"pad cleanup: sweep done"
		);
	}

	fn last_sweep() -> &'static Mutex<Option<Instant>> {
		static L: Mutex<Option<Instant>> = Mutex::new(None);
		&L
	}

	/// At host start: remove orphaned Duo HID devices and every recorded pad whose owner is
	/// gone. Blocking (SetupAPI, maybe `usbip`); run it off the UI/async threads.
	pub fn startup() {
		*last_sweep().lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
		sweep("host start");
	}

	/// Before plugging a pad in: the same sweep as [`startup`], at most every
	/// [`SWEEP_EVERY`], so a pad that died since start (another seat's Pulsar crashed)
	/// can't be picked over the new one.
	pub fn before_create() {
		{
			let mut l = last_sweep().lock().unwrap_or_else(|e| e.into_inner());
			if l.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
				return;
			}
			*l = Some(Instant::now());
		}
		sweep("pad create");
	}

	// ---- leases ----

	/// Duo ROOT ids claimed by live pads of this process (so two pads made close together
	/// don't record the same devnode).
	fn claimed() -> &'static Mutex<Vec<String>> {
		static C: Mutex<Vec<String>> = Mutex::new(Vec::new());
		&C
	}

	#[derive(Default)]
	struct LeaseState {
		rec: Option<PadRecord>,
		released: bool,
	}

	/// One live pad's record. Dropping it removes the record (after the pad's own `Drop`
	/// unplugged the device, when it is a field of the pad).
	pub struct PadLease(Option<Arc<Mutex<LeaseState>>>);

	impl PadLease {
		/// Records nothing.
		pub fn none() -> Self {
			Self(None)
		}

		fn record(rec: PadRecord) -> Self {
			if with_registry(|reg| reg.add(rec.clone())).is_none() {
				return Self::none();
			}
			Self(Some(Arc::new(Mutex::new(LeaseState {
				rec: Some(rec),
				released: false,
			}))))
		}

		/// A Duo controller whose ROOT devnode is known.
		pub fn duo_known(instance_id: &str) -> Self {
			if !is_duo_root_instance(instance_id) || !elevated() {
				return Self::none();
			}
			claimed()
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.push(instance_id.to_string());
			Self::record(PadRecord::Duo {
				instance_id: instance_id.to_string(),
				owner: me(),
			})
		}

		/// A Duo controller just created: find its ROOT devnode (new since `before`) on a
		/// background thread and record it. Records nothing when the devnode can't be told
		/// apart from another seat's new pad.
		pub fn duo_discover(before: Vec<String>) -> Self {
			if !elevated() {
				return Self::none();
			}
			let state = Arc::new(Mutex::new(LeaseState::default()));
			let st = state.clone();
			let spawned = std::thread::Builder::new()
				.name("duo-pad-record".into())
				.spawn(move || discover(before, st));
			if spawned.is_err() {
				return Self::none();
			}
			Self(Some(state))
		}

		/// A USB/IP pad served on 127.0.0.1:`tcp_port`.
		pub fn usbip(tcp_port: u16, vhci_port: Option<u32>) -> Self {
			if !elevated() {
				return Self::none();
			}
			Self::record(PadRecord::Usbip {
				tcp_port,
				vhci_port,
				owner: me(),
			})
		}
	}

	fn discover(before: Vec<String>, st: Arc<Mutex<LeaseState>>) {
		let deadline = Instant::now() + DISCOVER_TIMEOUT;
		let mut others: Vec<String> = Vec::new();
		while Instant::now() < deadline {
			if st.lock().unwrap_or_else(|e| e.into_inner()).released {
				return;
			}
			let now = duo_root_ids();
			let mut taken = claimed().lock().unwrap_or_else(|e| e.into_inner());
			let all_taken: Vec<String> = taken.iter().chain(others.iter()).cloned().collect();
			if let Some(id) = pick_new_root(&before, &now, &all_taken) {
				// Another Pulsar's record? Then it isn't ours.
				let foreign = with_registry(|reg| {
					reg.pads.iter().any(|p| {
						matches!(p, PadRecord::Duo { instance_id, owner }
							if instance_id.eq_ignore_ascii_case(&id) && owner.pid != me().pid)
					})
				})
				.unwrap_or(false);
				if foreign {
					others.push(id);
					continue;
				}
				taken.push(id.clone());
				drop(taken);
				let rec = PadRecord::Duo {
					instance_id: id.clone(),
					owner: me(),
				};
				let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
				if s.released {
					claimed()
						.lock()
						.unwrap_or_else(|e| e.into_inner())
						.retain(|c| !c.eq_ignore_ascii_case(&id));
					return;
				}
				with_registry(|reg| reg.add(rec.clone()));
				tracing::debug!(id, "pad cleanup: recorded Duo controller");
				s.rec = Some(rec);
				return;
			}
			drop(taken);
			std::thread::sleep(Duration::from_millis(150));
		}
		tracing::warn!("pad cleanup: couldn't tell this Duo controller's devnode apart; not recorded (a crash would leave it)");
	}

	impl Drop for PadLease {
		fn drop(&mut self) {
			let Some(st) = self.0.take() else { return };
			let rec = {
				let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
				s.released = true;
				s.rec.take()
			};
			let Some(rec) = rec else { return };
			if let PadRecord::Duo { instance_id, .. } = &rec {
				claimed()
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.retain(|c| !c.eq_ignore_ascii_case(instance_id));
			}
			with_registry(|reg| reg.remove(&rec));
		}
	}
}
