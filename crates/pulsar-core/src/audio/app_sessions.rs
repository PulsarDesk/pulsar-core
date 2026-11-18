//! The applications with an audio session on the host's render devices (Windows): what a
//! per-app mute of the session audio picks from (`DataMsg::AudioApps`).
//!
//! Every active render endpoint's `IAudioSessionManager2` is walked; each session gives its
//! process id (the system-sounds session and sessions of processes that are gone are left
//! out). A session is per process and per device, so one app can show several times; the
//! caller groups them by executable name.

use std::collections::HashMap;

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::{
	eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
	MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{
	CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
	OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// One process with an audio session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSessionApp {
	pub pid: u32,
	/// The executable's file name (`Discord.exe`).
	pub exe: String,
	/// The session is playing (not just open).
	pub active: bool,
}

/// Every process with an audio session on an active render device. Empty when the audio
/// service cannot be asked.
pub fn audio_session_apps() -> Vec<AudioSessionApp> {
	match sessions() {
		Ok(v) => v,
		Err(e) => {
			tracing::debug!("audio sessions: {e}");
			Vec::new()
		}
	}
}

fn sessions() -> windows::core::Result<Vec<AudioSessionApp>> {
	let mut out: Vec<AudioSessionApp> = Vec::new();
	// SAFETY: plain COM calls on interfaces this function owns; CoInitializeEx may report the
	// thread is already initialised (either model), which is fine for these calls.
	unsafe {
		let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
		let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
		let devs = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
		for i in 0..devs.GetCount()? {
			let Ok(dev) = devs.Item(i) else { continue };
			let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
			let Ok(list) = mgr.GetSessionEnumerator() else { continue };
			for j in 0..list.GetCount().unwrap_or(0) {
				let Ok(ctl) = list.GetSession(j) else { continue };
				let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
				if ctl2.IsSystemSoundsSession() == S_OK {
					continue;
				}
				let Ok(pid) = ctl2.GetProcessId() else { continue };
				if pid == 0 {
					continue;
				}
				let active = ctl.GetState().is_ok_and(|s| s == AudioSessionStateActive);
				if let Some(o) = out.iter_mut().find(|o| o.pid == pid) {
					o.active |= active;
					continue;
				}
				let Some(exe) = exe_name(pid) else { continue };
				out.push(AudioSessionApp { pid, exe, active });
			}
		}
	}
	Ok(out)
}

/// The executable file name of `pid` (`None` = gone, or not ours to ask).
pub fn exe_name(pid: u32) -> Option<String> {
	// SAFETY: the handle is closed below; the buffer outlives the call.
	unsafe {
		let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
		let mut buf = [0u16; 1024];
		let mut len = buf.len() as u32;
		let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
		let _ = CloseHandle(h);
		r.ok()?;
		let path = String::from_utf16_lossy(&buf[..len as usize]);
		path.rsplit(['\\', '/']).next().map(str::to_string).filter(|s| !s.is_empty())
	}
}

/// pid → parent pid for every process (empty when the snapshot fails).
pub fn parent_pids() -> HashMap<u32, u32> {
	let mut map = HashMap::new();
	// SAFETY: the snapshot handle is closed below; the entry is sized as the API requires.
	unsafe {
		let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
		let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
		if Process32FirstW(snap, &mut e).is_ok() {
			loop {
				map.insert(e.th32ProcessID, e.th32ParentProcessID);
				if Process32NextW(snap, &mut e).is_err() {
					break;
				}
			}
		}
		let _ = CloseHandle(snap);
	}
	map
}

/// Whether `pid` descends from `ancestor` (`parents` from [`parent_pids`]; cycles and reused
/// pids end the walk).
pub fn descends_from(pid: u32, ancestor: u32, parents: &HashMap<u32, u32>) -> bool {
	let mut cur = pid;
	for _ in 0..64 {
		match parents.get(&cur) {
			Some(&p) if p == ancestor => return true,
			Some(&p) if p != 0 && p != cur => cur = p,
			_ => return false,
		}
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ancestry_walk() {
		let parents: HashMap<u32, u32> = [(10, 5), (5, 1), (1, 0), (7, 7)].into_iter().collect();
		assert!(descends_from(10, 5, &parents));
		assert!(descends_from(10, 1, &parents));
		assert!(!descends_from(5, 10, &parents));
		assert!(!descends_from(7, 1, &parents), "a self-parent loop ends the walk");
		assert!(!descends_from(99, 1, &parents));
	}

	/// Lists the sessions of this machine (informative; `--ignored --nocapture`).
	#[test]
	#[ignore]
	fn lists_sessions() {
		for a in audio_session_apps() {
			eprintln!("{a:?}");
		}
	}
}
