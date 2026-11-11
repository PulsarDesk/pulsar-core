//! Windows: watch the default render endpoint — which one it is, its master volume and mute —
//! for the DualSense-aware host capture (`endpoint_route.rs`).
//!
//! Polled (the desktop host asks every 20 ms): the volume interface is cached per endpoint,
//! so a poll is three cheap `IAudioEndpointVolume` reads; which endpoint is the default is
//! re-asked only when the caller says so (`recheck`, every 100 ms there).

use super::endpoint_route::{is_dualsense_name, EndpointObservation};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
	CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// `ENDPOINT_HARDWARE_SUPPORT_VOLUME` (endpointvolume.h).
const HW_SUPPORT_VOLUME: u32 = 0x1;

struct Current {
	id: String,
	vol: IAudioEndpointVolume,
	is_dualsense: bool,
	hw_volume: bool,
}

/// One thread's watch on the default render endpoint. Keep it on the thread that made it
/// (COM, MTA).
pub struct DefaultRenderWatch {
	enumerator: IMMDeviceEnumerator,
	cur: Option<Current>,
}

impl DefaultRenderWatch {
	pub fn new() -> Result<Self, String> {
		unsafe {
			let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
			let enumerator: IMMDeviceEnumerator =
				CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
					.map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
			Ok(Self {
				enumerator,
				cur: None,
			})
		}
	}

	/// The default render endpoint now, or `None` (no default, or a read failed). `recheck`
	/// re-asks which endpoint is the default; otherwise the cached one is read.
	pub fn poll(&mut self, recheck: bool) -> Option<EndpointObservation> {
		if recheck || self.cur.is_none() {
			if let Err(e) = self.refresh() {
				tracing::debug!("default render watch: {e}");
				self.cur = None;
				return None;
			}
		}
		let cur = self.cur.as_ref()?;
		let read = unsafe {
			(|| -> windows::core::Result<(bool, f32, f32)> {
				Ok((
					cur.vol.GetMute()?.as_bool(),
					cur.vol.GetMasterVolumeLevel()?,
					cur.vol.GetMasterVolumeLevelScalar()?,
				))
			})()
		};
		match read {
			Ok((muted, level_db, scalar)) => Some(EndpointObservation {
				id: cur.id.clone(),
				is_dualsense: cur.is_dualsense,
				muted,
				level_db,
				scalar,
				hw_volume: cur.hw_volume,
			}),
			Err(e) => {
				tracing::debug!("default render watch: volume read failed: {e}");
				self.cur = None;
				None
			}
		}
	}

	fn refresh(&mut self) -> Result<(), String> {
		unsafe {
			let device = self
				.enumerator
				.GetDefaultAudioEndpoint(eRender, eConsole)
				.map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
			let pwstr = device.GetId().map_err(|e| format!("GetId: {e}"))?;
			let id = pwstr.to_string().map_err(|e| format!("GetId utf16: {e}"));
			CoTaskMemFree(Some(pwstr.0 as *const _));
			let id = id?;
			if self.cur.as_ref().is_some_and(|c| c.id == id) {
				return Ok(());
			}
			let vol: IAudioEndpointVolume = device
				.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
				.map_err(|e| format!("IAudioEndpointVolume: {e}"))?;
			let hw_volume = vol
				.QueryHardwareSupport()
				.map(|m| m & HW_SUPPORT_VOLUME != 0)
				.unwrap_or(false);
			let names = super::sink::device_name_fields(&device);
			let is_dualsense = is_dualsense_name(&names);
			tracing::info!(
				name = names
					.iter()
					.flatten()
					.next()
					.map(String::as_str)
					.unwrap_or("?"),
				is_dualsense,
				hw_volume,
				"default render endpoint"
			);
			self.cur = Some(Current {
				id,
				vol,
				is_dualsense,
				hw_volume,
			});
			Ok(())
		}
	}
}

#[cfg(test)]
mod tests {
	/// Live check on this machine: `cargo test -p pulsar-core --lib endpoint_watch -- --ignored --nocapture`.
	#[test]
	#[ignore]
	fn reads_the_default_endpoint() {
		let mut w = super::DefaultRenderWatch::new().unwrap();
		let o = w.poll(true).expect("a default render endpoint");
		println!("{o:?} gain={:?}", super::super::compensation_gain(&o));
		assert!(o.level_db <= 0.0 && (0.0..=1.0).contains(&o.scalar));
	}
}
