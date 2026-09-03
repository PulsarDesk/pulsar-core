//! Adaptive streaming — the shared client-side controller (`docs/adaptive-streaming.md`).
//!
//! * [`Controller`] — one pure decision per 2 s window: target wire rate, operating point
//!   (resolution × fps from the codec's [`ladder`]), encoder bitrate (net of FEC), and the
//!   host's loss-recovery mode. Fed with loss / keepalive RTT / per-frame arrival times.
//! * [`delay`] — the one-way delay-gradient estimator (trendline + over-use detector).
//! * [`fec_policy`] — how many packets one XOR parity covers for a measured loss.
//!
//! No I/O anywhere in here: the apps measure, call, and actuate (`StreamReq`).

pub mod delay;
pub mod fec_policy;
pub mod ladder;

mod controller;

pub use controller::{codec_from_wire, Config, Controller, Decision, Sample, Signals, FLOOR_KBPS};
pub use delay::DelayState;
pub use ladder::Point;
