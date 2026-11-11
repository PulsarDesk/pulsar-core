//! Plug in a virtual wired DualSense over USB/IP, feed it idle input, and print what the
//! OS / game sends back (output reports, haptics, speaker, stream and volume changes).
//!
//! Needs usbip-win2 and an elevated shell (Windows), or vhci-hcd loaded (Linux:
//! `sudo modprobe vhci-hcd`) and root or the `pulsar-vhci` group (no usbip CLI needed):
//!
//! ```text
//! cargo run -p pulsar-core --example ds5_virtual -- [seconds=60] [port=0]
//! ```
//!
//! Then open "Set up USB game controllers" (joy.cpl), the Windows sound settings, or a
//! game. The device is detached when the run ends; press Ctrl+C only as a last resort
//! (it skips the detach: then run `usbip detach -p <port>` by hand, or on Linux
//! `echo <port> | sudo tee /sys/devices/platform/vhci_hcd.0/detach`).
//!
//! Everything goes to stderr with timestamps. The wire trace (`PULSAR_DS5_TRACE`) is on
//! by default here: every control request and its answer, SET_INTERFACE, unlinks, the
//! first iso URBs per stream and per-endpoint URB counts each second. Set
//! `PULSAR_DS5_TRACE=0` to silence it.

use std::time::{Duration, Instant};

use pulsar_core::input::ds5::usbip::trace_line;
use pulsar_core::input::ds5::virtual_pad::{
	idle_input, Ds5HostEvent, VirtualDs5, VirtualDs5Config,
};

macro_rules! log {
	($($a:tt)*) => { trace_line(format_args!($($a)*)) };
}

fn main() -> std::io::Result<()> {
	if std::env::var_os("PULSAR_DS5_TRACE").is_none() {
		std::env::set_var("PULSAR_DS5_TRACE", "1");
	}
	let mut args = std::env::args().skip(1);
	let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
	let port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);

	let (pad, rx) = VirtualDs5::start(VirtualDs5Config {
		port,
		..Default::default()
	})?;
	log!(
		"virtual DualSense on 127.0.0.1:{} attached={} — running {secs} s",
		pad.port(),
		pad.attached()
	);

	let input = idle_input();
	let end = Instant::now() + Duration::from_secs(secs);
	let mut last_input = Instant::now();
	let mut last_stats = Instant::now();
	let (mut haptic_frames, mut haptic_peak, mut speaker_samples, mut speaker_peak) =
		(0u32, 0i32, 0usize, 0f32);
	let mut outputs = 0u32;
	while Instant::now() < end {
		// 250 Hz idle input, like a wired pad with nothing pressed.
		if last_input.elapsed() >= Duration::from_millis(4) {
			pad.set_input(&input);
			last_input = Instant::now();
		}
		match rx.recv_timeout(Duration::from_millis(2)) {
			Ok(Ds5HostEvent::Output(s)) => {
				outputs += 1;
				if outputs <= 20 || outputs % 100 == 0 {
					log!("output #{outputs}: {:02x?}", &s.0[..47]);
				}
			}
			Ok(Ds5HostEvent::Haptics(f)) => {
				haptic_frames += 1;
				for &b in &f {
					haptic_peak = haptic_peak.max((b as i8 as i32).abs());
				}
			}
			Ok(Ds5HostEvent::Speaker(pcm)) => {
				speaker_samples += pcm.len();
				for v in pcm {
					speaker_peak = speaker_peak.max(v.abs());
				}
			}
			Ok(e) => log!("event {e:?}"),
			Err(_) => {}
		}
		if last_stats.elapsed() >= Duration::from_secs(1) {
			if haptic_frames > 0 || speaker_samples > 0 {
				log!(
					"last 1 s: haptics {haptic_frames} frames (peak {haptic_peak}/127), \
					 speaker {} frames (peak {speaker_peak:.3})",
					speaker_samples / 2
				);
			}
			(haptic_frames, haptic_peak, speaker_samples, speaker_peak) = (0, 0, 0, 0.0);
			last_stats = Instant::now();
		}
	}
	log!("done; detaching");
	drop(pad);
	Ok(())
}
