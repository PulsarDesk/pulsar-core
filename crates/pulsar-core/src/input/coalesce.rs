//! Client-side coalescing of high-rate pointer input before it goes on the session.
//!
//! A 1-8 kHz gaming mouse produces one relative-motion report per poll; sending each as its
//! own encrypted datagram means 1000-8000 packets/s upstream, which queues at a relay or a
//! thin uplink and then arrives as aim lag / jumps. [`MotionCoalescer`] caps each motion
//! kind at one packet per [`MOTION_WINDOW`] the way Moonlight batches mouse input, WITHOUT
//! delaying the movement that starts a burst:
//!
//! - each motion kind has a small token bucket ([`MOTION_BURST`] packets, refilled one per
//!   [`MOTION_WINDOW`]): a motion event with nothing pending and a token goes out
//!   IMMEDIATELY, so an ordinary 1 kHz mouse — even with USB / hook jitter of several hundred
//!   µs, or two reports delivered together — is never held back; only a sustained stream
//!   above ~1.25 kHz runs the bucket dry;
//! - one arriving without a token is merged into a pending event (relative / scroll deltas
//!   add up — no distance is lost; an absolute position replaces the older one) and goes out
//!   as soon as the next token is due: on the next motion event past it, or from the
//!   caller's timer at [`MotionCoalescer::deadline`];
//! - events that queued up before the caller could read them (a busy loop, a hook
//!   delivering in a batch) go through [`MotionCoalescer::push_backlog`]: merged, but never
//!   held — they are late already;
//! - any other event (button, key, pen, pad…) first flushes what is pending, so the host
//!   sees exactly the original order: a click lands where the pointer was.
//!
//! Pure (the caller passes `now`), so it's unit-tested on every target.

use std::time::{Duration, Instant};

use crate::service::InputEvent;

/// Average spacing of two packets of the same motion kind (the bucket's refill period). A
/// bit under 1 ms so a 1 kHz mouse never waits; faster mice drop to ≤ ~1250 packets/s.
pub const MOTION_WINDOW: Duration = Duration::from_micros(800);
/// Packets of one motion kind that may go out back to back (the bucket's depth): absorbs
/// jitter and pairs of reports delivered together without holding any of them.
pub const MOTION_BURST: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
	Relative,
	Absolute,
	Scroll,
}

fn kind(ev: &InputEvent) -> Option<Kind> {
	match ev {
		InputEvent::PointerRelative { .. } => Some(Kind::Relative),
		InputEvent::PointerMotion { .. } => Some(Kind::Absolute),
		InputEvent::Scroll { .. } => Some(Kind::Scroll),
		_ => None,
	}
}

/// See the module docs.
#[derive(Debug)]
pub struct MotionCoalescer {
	window: Duration,
	/// The held-back event (already merged) and when it is due.
	pending: Option<(InputEvent, Instant)>,
	/// Per kind (index = `Kind as usize`): the bucket's "theoretical arrival time" (GCRA) —
	/// a packet may go at `now` when `now >= tat - (burst - 1) × window`; each send moves it
	/// to `max(tat, now) + window`. `None` = full bucket.
	tat: [Option<Instant>; 3],
}

impl Default for MotionCoalescer {
	fn default() -> Self {
		Self::new(MOTION_WINDOW)
	}
}

impl MotionCoalescer {
	pub fn new(window: Duration) -> Self {
		Self { window, pending: None, tat: [None; 3] }
	}

	/// When the next packet of kind `k` may go (`None` = now).
	fn next_slot(&self, k: Kind) -> Option<Instant> {
		self.tat[k as usize].and_then(|t| t.checked_sub(self.window * (MOTION_BURST - 1)))
	}

	fn sent(&mut self, k: Kind, now: Instant) {
		let t = self.tat[k as usize].map_or(now, |t| t.max(now));
		self.tat[k as usize] = Some(t + self.window);
	}

	/// When the pending event is due (`None` = nothing pending). The caller wakes then and
	/// calls [`poll`](Self::poll).
	pub fn deadline(&self) -> Option<Instant> {
		self.pending.as_ref().map(|(_, due)| *due)
	}

	/// The pending event if its window has closed by `now`.
	pub fn poll(&mut self, now: Instant) -> Option<InputEvent> {
		match self.pending {
			Some((_, due)) if now >= due => self.take(now),
			_ => None,
		}
	}

	/// Whatever is pending, regardless of its window (session end, a forced flush).
	pub fn flush(&mut self, now: Instant) -> Option<InputEvent> {
		self.take(now)
	}

	fn take(&mut self, now: Instant) -> Option<InputEvent> {
		let (ev, _) = self.pending.take()?;
		if let Some(k) = kind(&ev) {
			self.sent(k, now);
		}
		Some(ev)
	}

	/// Feed one event; `out` receives what to send now, in order.
	pub fn push(&mut self, now: Instant, ev: InputEvent, out: &mut Vec<InputEvent>) {
		let Some(k) = kind(&ev) else {
			// An edge / pen / pad event: everything before it goes first.
			out.extend(self.take(now));
			out.push(ev);
			return;
		};
		if let Some((p, due)) = self.pending.as_mut() {
			if kind(p) == Some(k) {
				merge(p, ev);
				if now >= *due {
					out.extend(self.take(now));
				}
				return;
			}
			// A different motion kind: keep the order (the older one first).
			out.extend(self.take(now));
		}
		match self.next_slot(k) {
			Some(due) if now < due => self.pending = Some((ev, due)),
			_ => {
				self.sent(k, now);
				out.push(ev);
			}
		}
	}

	/// Feed events that were ALREADY waiting when the caller read the first of them (a
	/// channel backlog after the loop was busy, a hook batch). They merge like [`push`]
	/// (fewer packets), but whatever that leaves pending goes out now instead of waiting for
	/// a token: the input is late already, holding the burst's final position for the timer
	/// would only add to it. A single event is a plain [`push`].
	///
	/// [`push`]: Self::push
	pub fn push_backlog(&mut self, now: Instant, evs: impl IntoIterator<Item = InputEvent>, out: &mut Vec<InputEvent>) {
		let mut n = 0usize;
		for ev in evs {
			self.push(now, ev, out);
			n += 1;
		}
		if n > 1 {
			out.extend(self.take(now));
		}
	}
}

fn merge(into: &mut InputEvent, ev: InputEvent) {
	match (into, ev) {
		(InputEvent::PointerRelative { dx, dy }, InputEvent::PointerRelative { dx: x, dy: y })
		| (InputEvent::Scroll { dx, dy }, InputEvent::Scroll { dx: x, dy: y }) => {
			*dx += x;
			*dy += y;
		}
		(p @ InputEvent::PointerMotion { .. }, e @ InputEvent::PointerMotion { .. }) => *p = e,
		_ => {}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rel(dx: f64, dy: f64) -> InputEvent {
		InputEvent::PointerRelative { dx, dy }
	}
	fn us(t0: Instant, n: u64) -> Instant {
		t0 + Duration::from_micros(n)
	}

	#[test]
	fn first_motion_goes_out_immediately() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		c.push(t0, rel(1.0, 2.0), &mut out);
		assert_eq!(out, vec![rel(1.0, 2.0)]);
		assert_eq!(c.deadline(), None);
	}

	#[test]
	fn a_1khz_mouse_is_never_held_back() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		// 1 kHz with up to ±450 µs jitter (USB / hook scheduling): consecutive reports land
		// as close as 150 µs apart, which a fixed 800 µs spacing would have held.
		for (i, j) in [0i64, 90, -100, 50, -80, 100, -450, 400, -300, 0, 450, -400].iter().enumerate() {
			let t = us(t0, (i as i64 * 1000 + 100 + j) as u64);
			c.push(t, rel(1.0, 0.0), &mut out);
			assert_eq!(out.len(), i + 1, "sample {i} was held back");
		}
	}

	#[test]
	fn an_8khz_mouse_is_batched_without_losing_distance() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		// 80 reports of (1, -1) at 125 µs = 10 ms of motion.
		for i in 0..80u64 {
			c.push(us(t0, i * 125), rel(1.0, -1.0), &mut out);
		}
		if let Some(e) = c.flush(us(t0, 10_000)) {
			out.push(e);
		}
		// ≤ 10 ms / 0.8 ms + the bucket depth + the flushed tail.
		assert!(out.len() <= 16, "{} packets for 10 ms", out.len());
		let (sx, sy) = out.iter().fold((0.0, 0.0), |(x, y), e| match e {
			InputEvent::PointerRelative { dx, dy } => (x + dx, y + dy),
			_ => panic!("{e:?}"),
		});
		assert_eq!((sx, sy), (80.0, -80.0));
	}

	#[test]
	fn the_tail_is_due_within_a_window() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		c.push(t0, rel(1.0, 0.0), &mut out);
		c.push(us(t0, 100), rel(1.0, 0.0), &mut out); // the bucket's second token
		c.push(us(t0, 200), rel(2.0, 0.0), &mut out);
		c.push(us(t0, 300), rel(3.0, 0.0), &mut out);
		assert_eq!(out.len(), 2);
		assert_eq!(c.deadline(), Some(t0 + MOTION_WINDOW));
		assert_eq!(c.poll(us(t0, 500)), None);
		assert_eq!(c.poll(t0 + MOTION_WINDOW), Some(rel(5.0, 0.0)));
		assert_eq!(c.deadline(), None);
	}

	#[test]
	fn an_edge_flushes_first_so_the_click_lands_where_the_pointer_is() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		c.push(t0, rel(1.0, 0.0), &mut out);
		c.push(us(t0, 50), rel(2.0, 0.0), &mut out);
		c.push(us(t0, 100), rel(4.0, 0.0), &mut out); // held
		let click = InputEvent::PointerButton { button: 0, down: true };
		c.push(us(t0, 150), click, &mut out);
		assert_eq!(out, vec![rel(1.0, 0.0), rel(2.0, 0.0), rel(4.0, 0.0), click]);
		assert_eq!(c.deadline(), None);
	}

	#[test]
	fn scroll_sums_and_absolute_keeps_the_latest() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		c.push(t0, InputEvent::Scroll { dx: 0.0, dy: 10.0 }, &mut out);
		c.push(t0, InputEvent::Scroll { dx: 0.0, dy: 10.0 }, &mut out);
		c.push(us(t0, 100), InputEvent::Scroll { dx: 0.0, dy: 2.5 }, &mut out);
		c.push(us(t0, 200), InputEvent::Scroll { dx: 1.0, dy: 2.5 }, &mut out);
		assert_eq!(c.flush(us(t0, 300)), Some(InputEvent::Scroll { dx: 1.0, dy: 5.0 }));
		out.clear();
		c.push(t0, InputEvent::PointerMotion { x: 0.1, y: 0.1 }, &mut out);
		c.push(t0, InputEvent::PointerMotion { x: 0.1, y: 0.1 }, &mut out);
		c.push(us(t0, 100), InputEvent::PointerMotion { x: 0.2, y: 0.2 }, &mut out);
		c.push(us(t0, 200), InputEvent::PointerMotion { x: 0.3, y: 0.4 }, &mut out);
		assert_eq!(c.flush(us(t0, 300)), Some(InputEvent::PointerMotion { x: 0.3, y: 0.4 }));
	}

	#[test]
	fn mixed_kinds_keep_their_order() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		c.push(t0, rel(1.0, 0.0), &mut out);
		c.push(us(t0, 50), rel(1.0, 0.0), &mut out);
		c.push(us(t0, 100), rel(1.0, 0.0), &mut out); // pending
		c.push(us(t0, 200), InputEvent::Scroll { dx: 0.0, dy: 5.0 }, &mut out);
		// The pending motion went first, then the (first-of-its-kind) scroll immediately.
		assert_eq!(out, vec![rel(1.0, 0.0), rel(1.0, 0.0), rel(1.0, 0.0), InputEvent::Scroll { dx: 0.0, dy: 5.0 }]);
	}

	#[test]
	fn a_backlog_is_merged_but_never_held() {
		let t0 = Instant::now();
		let mut c = MotionCoalescer::default();
		let mut out = vec![];
		// The loop was busy: three 1 kHz reports come out of the channel at the same instant.
		c.push_backlog(t0, [rel(1.0, 0.0), rel(2.0, 0.0), rel(3.0, 0.0)], &mut out);
		// Two tokens go as they are, the third is not held for the timer.
		assert_eq!(out, vec![rel(1.0, 0.0), rel(2.0, 0.0), rel(3.0, 0.0)]);
		assert_eq!(c.deadline(), None);
		// A longer backlog right after: merged into one packet, sent at once.
		out.clear();
		c.push_backlog(us(t0, 10), (0..6).map(|_| rel(1.0, 1.0)), &mut out);
		assert_eq!(out, vec![rel(6.0, 6.0)]);
		assert_eq!(c.deadline(), None);
		// A single event is a plain push: held while the bucket is dry.
		out.clear();
		c.push_backlog(us(t0, 20), [rel(1.0, 0.0)], &mut out);
		assert!(out.is_empty());
		assert!(c.deadline().is_some());
	}
}
