//! Adaptive streaming — the shared client-side controller (`docs/adaptive-streaming.md`).
//!
//! * [`Controller`] — one pure decision per 2 s window: target wire rate, operating point
//!   (resolution × fps from the codec's [`ladder`]), encoder bitrate (net of FEC), and the
//!   host's loss-recovery mode. Fed with loss / keepalive RTT / per-frame arrival times.
//! * [`delay`] — the one-way delay-gradient estimator (trendline + over-use detector).
//! * [`fec_policy`] — how many packets one XOR parity covers for a measured loss.
//! * [`probe`] — the session-start bandwidth probe (packet trains → the start rate).
//! * [`repair`] — the client's video repair loop (seq gaps → reorder / FEC waits → NACK,
//!   re-NACK, write-off; window counters incl. goodput; FEC share; keyframe pacing).
//! * [`caps`] — the automatic ceiling (`auto_cap_kbps`) and the live path's cap.
//! * [`memory`] — the per-peer, per-path-class memory of the last clean rate.
//! * [`pong`] — keepalive RTT: the uneven Ping cadence and the Pong matcher.
//!
//! No I/O anywhere in here (except `memory`'s best-effort JSON file at a path the app gives):
//! the apps measure, call, and actuate (`StreamReq`).

pub mod caps;
pub mod delay;
pub mod fec_policy;
pub mod ladder;
pub mod memory;
pub mod pong;
pub mod probe;
pub mod repair;

mod controller;

pub use controller::{
	codec_from_wire, rate_floor_for_fps, Config, Controller, Decision, Sample, Signals, FLOOR_KBPS, KBPS_PER_FPS_FLOOR,
	START_KBPS_NO_MEMORY,
};
pub use delay::DelayState;
pub use ladder::Point;
