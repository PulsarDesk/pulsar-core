//! Host-side input permission checks: whether the client's keyboard / mouse / pen / touch can
//! reach this host's desktop at all. Every OS lets a host without the right permission inject
//! "successfully" while the input goes nowhere:
//!
//! * Linux: `/dev/uinput` is missing or not writable for this user (no udev `uaccess` rule,
//!   e.g. a stock distro without `steam-devices`, or the AppImage, which installs nothing).
//! * macOS: the app lacks the Accessibility grant; `CGEventPost` then silently no-ops.
//! * Windows: a non-elevated host cannot drive a window of a higher-integrity process
//!   (Task Manager, installers, any "Run as administrator" app); UIPI drops the input while
//!   `SendInput` still reports success. Only the window in front matters, so this is a
//!   live check ([`foreground_window_blocked`]), not a preflight.
//!
//! The host reports a failure to the client as `DataMsg::InputUnavailable` (the
//! [`InputFailure::as_wire`] token) and shows its own notice.

/// Why the client's desktop input cannot reach this host (or the window in front).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputFailure {
	/// Linux: `/dev/uinput` is missing or not writable for this user.
	UinputDenied,
	/// macOS: the Accessibility permission is not granted to Pulsar.
	MacAccessibility,
	/// Windows: the window in front belongs to a process with a higher integrity level
	/// (elevated / "Run as administrator") than this host, so UIPI drops the input.
	WinElevatedWindow,
	/// Android: the control (Accessibility) service is not enabled.
	AndroidControlOff,
	/// Anything else (the host log has the detail).
	Other,
}

impl InputFailure {
	/// Stable wire token (the client localizes it; unknown tokens fall back to a generic text).
	pub fn as_wire(self) -> &'static str {
		match self {
			Self::UinputDenied => "uinput_denied",
			Self::MacAccessibility => "mac_accessibility",
			Self::WinElevatedWindow => "win_elevated",
			Self::AndroidControlOff => "android_control_off",
			Self::Other => "unavailable",
		}
	}

	/// Inverse of [`as_wire`](Self::as_wire); unknown tokens (a newer host) map to `Other`.
	pub fn from_wire(s: &str) -> Self {
		match s {
			"uinput_denied" => Self::UinputDenied,
			"mac_accessibility" => Self::MacAccessibility,
			"win_elevated" => Self::WinElevatedWindow,
			"android_control_off" => Self::AndroidControlOff,
			_ => Self::Other,
		}
	}
}

/// Whether this host can inject desktop input at all (host start, session start). `None` =
/// no known problem. Cheap (one `open` on Linux, one TCC query on macOS); never prompts.
/// Windows has no process-wide failure (see [`foreground_window_blocked`]); Android checks
/// its control service in the app.
pub fn preflight() -> Option<InputFailure> {
	#[cfg(target_os = "linux")]
	{
		if let Err(e) = linux::open_uinput() {
			tracing::warn!(err = %e, "remote input: /dev/uinput is not writable; client input will not reach this desktop");
			return Some(InputFailure::UinputDenied);
		}
		None
	}
	#[cfg(target_os = "macos")]
	{
		if !mac::ax_trusted() {
			tracing::warn!("remote input: Accessibility permission missing; client input will not reach this desktop");
			return Some(InputFailure::MacAccessibility);
		}
		None
	}
	#[cfg(not(any(target_os = "linux", target_os = "macos")))]
	{
		None
	}
}

#[cfg(target_os = "linux")]
mod linux {
	/// Open the uinput control node for writing, as the uinput backend does (`evdev` opens
	/// `/dev/uinput`; some systems only have `/dev/input/uinput`). Opening creates no device.
	pub(super) fn open_uinput() -> std::io::Result<()> {
		let open = |p: &str| std::fs::OpenOptions::new().write(true).open(p).map(drop);
		open("/dev/uinput").or_else(|e| open("/dev/input/uinput").map_err(|_| e))
	}
}

#[cfg(target_os = "macos")]
mod mac {
	#[link(name = "ApplicationServices", kind = "framework")]
	extern "C" {
		fn AXIsProcessTrusted() -> u8;
	}

	/// This process holds the Accessibility grant (never prompts).
	pub(super) fn ax_trusted() -> bool {
		unsafe { AXIsProcessTrusted() != 0 }
	}
}

/// Windows: the window in front, when it belongs to a process whose integrity level is above
/// this process's (an elevated app while the host is not elevated; a SYSTEM window while the
/// host is elevated). UIPI then drops `SendInput` / `PostMessage` to it although both report
/// success. Returns the window handle, so the caller can tell one window from the next;
/// `None` = not blocked or unknown (no foreground window, our own window, a process we may
/// not even query).
///
/// The integrity level is read from the target's token when this process may open it. An
/// elevated process's token is usually closed to a non-elevated caller; then the process
/// object's mandatory "no read up" policy tells: a process we may query but not read
/// (`PROCESS_VM_READ` denied) runs above our integrity level.
#[cfg(windows)]
pub fn foreground_window_blocked() -> Option<isize> {
	win::foreground_window_blocked()
}

#[cfg(windows)]
mod win {
	use std::sync::OnceLock;

	use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE};
	use windows_sys::Win32::Security::{
		GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
		TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
	};
	use windows_sys::Win32::System::Threading::{
		GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
		PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetForegroundWindow, GetWindowThreadProcessId,
	};

	/// `SECURITY_MANDATORY_HIGH_RID`.
	const HIGH_RID: u32 = 0x3000;

	/// The integrity RID of `token` (0x2000 medium, 0x3000 high, 0x4000 system).
	unsafe fn token_rid(token: HANDLE) -> Option<u32> {
		let mut len = 0u32;
		GetTokenInformation(
			token,
			TokenIntegrityLevel,
			std::ptr::null_mut(),
			0,
			&mut len,
		);
		if len == 0 {
			return None;
		}
		// u64 storage keeps the TOKEN_MANDATORY_LABEL (pointer + u32) aligned.
		let mut buf = vec![0u64; (len as usize).div_ceil(8)];
		if GetTokenInformation(
			token,
			TokenIntegrityLevel,
			buf.as_mut_ptr().cast(),
			len,
			&mut len,
		) == 0
		{
			return None;
		}
		let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
		let sid = label.Label.Sid;
		if sid.is_null() {
			return None;
		}
		let count = *GetSidSubAuthorityCount(sid);
		if count == 0 {
			return None;
		}
		Some(*GetSidSubAuthority(sid, count as u32 - 1))
	}

	unsafe fn process_rid(process: HANDLE) -> Option<u32> {
		let mut token: HANDLE = std::ptr::null_mut();
		if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
			return None;
		}
		let rid = token_rid(token);
		CloseHandle(token);
		rid
	}

	fn own_rid() -> u32 {
		static RID: OnceLock<u32> = OnceLock::new();
		// Unknown → assume medium (the common non-elevated case).
		*RID.get_or_init(|| unsafe { process_rid(GetCurrentProcess()) }.unwrap_or(0x2000))
	}

	/// Whether process `pid` runs above `mine` (`None` = cannot tell).
	fn above(pid: u32, mine: u32) -> Option<bool> {
		unsafe {
			let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
			if h.is_null() {
				return None;
			}
			let rid = process_rid(h);
			CloseHandle(h);
			if let Some(rid) = rid {
				return Some(rid > mine);
			}
			// Token closed to us. Below high integrity, "query yes / read no" is the
			// mandatory no-read-up policy of a higher-integrity process object.
			if mine >= HIGH_RID {
				return None;
			}
			let r = OpenProcess(PROCESS_VM_READ, 0, pid);
			if !r.is_null() {
				CloseHandle(r);
				return Some(false);
			}
			(GetLastError() == ERROR_ACCESS_DENIED).then_some(true)
		}
	}

	pub(super) fn foreground_window_blocked() -> Option<isize> {
		unsafe {
			let hwnd = GetForegroundWindow();
			if hwnd.is_null() {
				return None;
			}
			let mut pid = 0u32;
			GetWindowThreadProcessId(hwnd, &mut pid);
			if pid == 0 || pid == GetCurrentProcessId() {
				return None;
			}
			above(pid, own_rid())?.then_some(hwnd as isize)
		}
	}

	#[cfg(test)]
	mod tests {
		#[test]
		fn own_process_is_never_above_itself() {
			let pid = unsafe { super::GetCurrentProcessId() };
			assert_eq!(super::above(pid, super::own_rid()), Some(false));
		}

		#[test]
		fn foreground_check_does_not_panic() {
			// The result depends on the desktop; it only must not crash or hang.
			let _ = super::foreground_window_blocked();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::InputFailure;

	#[test]
	fn wire_tokens_round_trip_and_unknown_is_other() {
		for f in [
			InputFailure::UinputDenied,
			InputFailure::MacAccessibility,
			InputFailure::WinElevatedWindow,
			InputFailure::AndroidControlOff,
			InputFailure::Other,
		] {
			assert_eq!(InputFailure::from_wire(f.as_wire()), f);
		}
		assert_eq!(
			InputFailure::from_wire("from_the_future"),
			InputFailure::Other
		);
		assert_eq!(InputFailure::from_wire(""), InputFailure::Other);
	}

	#[test]
	fn preflight_does_not_panic() {
		let _ = super::preflight();
	}
}
