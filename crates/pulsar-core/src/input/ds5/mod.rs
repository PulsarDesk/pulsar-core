//! DualSense (PS5) passthrough: protocol helpers, the client-side physical pad owner and
//! the host-side virtual wired DualSense. Design: `docs/dualsense.md`.
//!
//! Protocol knowledge comes from the Linux `hid-playstation` driver, ZerkSense5 (MIT,
//! florinp93) and SAxense (egormanga, https://apps.sdore.me/SAxense) / DS5Dongle (awalol)
//! (MPL-2.0); the code here is written from their documented layouts. No Sony SDK code or
//! firmware is used.

use serde::{Deserialize, Serialize};

pub mod attach;
pub mod bt_audio;
pub mod crc;
pub mod degrade;
pub mod device;
pub mod duo;
pub mod haptics;
pub mod mic;
pub mod pad;
pub mod report;
pub mod rumble;
pub mod speaker;
pub mod usb_audio;
pub mod usbip;
pub mod vhci;
pub mod virtual_pad;
pub mod wire;

/// Which DualSense features the client lets through to its physical pad. The host also
/// uses it to skip work for features the client turned off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ds5Features {
	pub triggers: bool,
	pub rumble: bool,
	pub leds: bool,
	pub haptics: bool,
	pub speaker: bool,
	pub mic: bool,
}

impl Ds5Features {
	/// Everything on.
	pub const fn all() -> Self {
		Self {
			triggers: true,
			rumble: true,
			leds: true,
			haptics: true,
			speaker: true,
			mic: true,
		}
	}
}

impl Default for Ds5Features {
	fn default() -> Self {
		Self::all()
	}
}
