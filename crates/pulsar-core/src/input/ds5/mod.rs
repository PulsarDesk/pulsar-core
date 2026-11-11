//! DualSense (PS5) passthrough: protocol helpers, the client-side physical pad owner and
//! the host-side virtual wired DualSense. Design: `docs/dualsense.md`.
//!
//! Protocol knowledge comes from the Linux `hid-playstation` driver, ZerkSense5 (MIT,
//! florinp93) and SAxense / DS5Dongle (MPL-2.0); no Sony SDK code or firmware is used.

pub mod crc;
