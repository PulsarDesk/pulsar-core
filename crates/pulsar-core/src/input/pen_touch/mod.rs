//! Pen (stylus) and multi-touch input.
//!
//! - **Wire types**: [`PenState`] and [`TouchFrame`] travel inside
//!   [`InputEvent::Pen`](crate::service::InputEvent::Pen) /
//!   [`InputEvent::Touch`](crate::service::InputEvent::Touch). Both are *state* messages (the
//!   whole pen / every finger on the glass), not edges: a lost datagram is healed by the next
//!   one, and the client re-sends the state every [`KEEPALIVE`] while anything is down or
//!   hovering, so a host can lift whatever has gone [`SILENCE`] without news.
//! - **Host**: [`Driver`] turns the stream of states into edges for a platform [`Sink`]
//!   (Windows synthetic pointer devices, Linux uinput tablet + multitouch, macOS CGEvent
//!   tablet events and touch-as-mouse) and runs it on a worker thread ([`PenTouch`]), so the
//!   Windows keep-alive re-injection and the silence timeout fire without input arriving.
//! - **Mapping to a mouse** ([`mouse`]): where no real pen/touch path exists (touch on a macOS
//!   host, a host that does not advertise `FEAT_PEN` / `FEAT_TOUCH`).
//! - **Client** ([`client::ClientPenTouch`]): the per-session gate between the UI / native
//!   capture and the wire (feature check, mouse fallback, keep-alive re-send).
//!
//! Coordinates are normalized (0..1) over the streamed screen, like `PointerMotion`.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub mod client;
pub mod mouse;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{pointer_info, LocalDevices, PointerBackend, SynthContact, WinSink};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::UinputSink;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{display_bounds as macos_display_bounds, MacSink};

/// Most simultaneous touch contacts carried and injected (Windows synthetic touch devices,
/// the Linux multitouch slots). Extra fingers are ignored.
pub const MAX_TOUCH: usize = 10;

/// How often a client re-sends the current pen / touch state while something is in range.
pub const KEEPALIVE: Duration = Duration::from_millis(250);

/// A host lifts the pen / every finger after this long without a pen / touch message (the
/// client vanished, or its final "lifted" state was lost). Four keep-alives fit inside.
pub const SILENCE: Duration = Duration::from_millis(1000);

/// One stylus sample: the full pen state (not an edge).
///
/// `in_range = false` means the pen left proximity (everything else is then ignored);
/// `in_range && !contact` is hover. Every field past `x`/`y` is `#[serde(default)]` so the
/// struct can grow without breaking older peers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PenState {
	pub x: f32,
	pub y: f32,
	/// Tip pressure 0..1 (0 while hovering).
	#[serde(default)]
	pub pressure: f32,
	/// Tilt in degrees, -90..90 (PointerEvent `tiltX` / `tiltY`: positive = toward +x / +y).
	#[serde(default)]
	pub tilt_x: f32,
	#[serde(default)]
	pub tilt_y: f32,
	/// Barrel rotation in degrees, 0..359.
	#[serde(default)]
	pub twist: f32,
	/// The pen is in proximity (hovering or touching).
	#[serde(default)]
	pub in_range: bool,
	/// The tip (or the eraser end) touches the surface.
	#[serde(default)]
	pub contact: bool,
	/// The eraser end is the active tool.
	#[serde(default)]
	pub eraser: bool,
	/// The barrel (side) button is held.
	#[serde(default)]
	pub barrel: bool,
}

fn finite_or(v: f32, d: f32) -> f32 {
	if v.is_finite() {
		v
	} else {
		d
	}
}

impl PenState {
	/// Clamp every field into its range (a peer's JSON is untrusted) and normalize the
	/// out-of-range state, so sinks only ever see consistent values.
	pub fn sanitized(self) -> Self {
		if !self.in_range {
			return Self {
				x: finite_or(self.x, 0.0).clamp(0.0, 1.0),
				y: finite_or(self.y, 0.0).clamp(0.0, 1.0),
				..Self::default()
			};
		}
		let contact = self.contact;
		Self {
			x: finite_or(self.x, 0.0).clamp(0.0, 1.0),
			y: finite_or(self.y, 0.0).clamp(0.0, 1.0),
			pressure: if contact { finite_or(self.pressure, 0.5).clamp(0.0, 1.0) } else { 0.0 },
			tilt_x: finite_or(self.tilt_x, 0.0).clamp(-90.0, 90.0),
			tilt_y: finite_or(self.tilt_y, 0.0).clamp(-90.0, 90.0),
			twist: finite_or(self.twist, 0.0).rem_euclid(360.0),
			in_range: true,
			contact,
			eraser: self.eraser,
			barrel: self.barrel,
		}
	}
}

/// One finger on the glass. `id` is the client's contact id (stable while the finger is
/// down; any value); the host maps it onto its own slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TouchPoint {
	pub id: u32,
	pub x: f32,
	pub y: f32,
	/// Contact pressure 0..1; 0 = the client does not know.
	#[serde(default)]
	pub pressure: f32,
}

/// Every finger currently on the client's screen (empty = all lifted). Fixed capacity so
/// [`InputEvent`](crate::service::InputEvent) stays `Copy`; serialized as a plain JSON
/// array of the live points. Built with [`TouchFrame::new`], which drops duplicate ids and
/// anything past [`MAX_TOUCH`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TouchFrame {
	n: u8,
	pts: [TouchPoint; MAX_TOUCH],
}

impl TouchFrame {
	pub fn new(points: &[TouchPoint]) -> Self {
		let mut f = Self::default();
		for p in points {
			if f.n as usize == MAX_TOUCH {
				break;
			}
			if f.points().iter().any(|q| q.id == p.id) {
				continue;
			}
			f.pts[f.n as usize] = TouchPoint {
				id: p.id,
				x: finite_or(p.x, 0.0).clamp(0.0, 1.0),
				y: finite_or(p.y, 0.0).clamp(0.0, 1.0),
				pressure: finite_or(p.pressure, 0.0).clamp(0.0, 1.0),
			};
			f.n += 1;
		}
		f
	}
	/// The live contacts, in the client's order (the first is the oldest finger).
	pub fn points(&self) -> &[TouchPoint] {
		&self.pts[..self.n as usize]
	}
	pub fn len(&self) -> usize {
		self.n as usize
	}
	pub fn is_empty(&self) -> bool {
		self.n == 0
	}
	pub fn get(&self, id: u32) -> Option<&TouchPoint> {
		self.points().iter().find(|p| p.id == id)
	}
}

impl Serialize for TouchFrame {
	fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
		self.points().serialize(s)
	}
}

impl<'de> Deserialize<'de> for TouchFrame {
	fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		let v = Vec::<TouchPoint>::deserialize(d)?;
		Ok(Self::new(&v))
	}
}

/// What happened to one host slot in a touch update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contact {
	Down,
	Move,
	Up,
}

/// One slot's change: Windows injects every one of them in a single call (all active
/// contacts must be present in each injection), Linux writes them as one multitouch frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotChange {
	pub slot: usize,
	pub phase: Contact,
	/// Where the contact is (for `Up`: where it was last).
	pub pt: TouchPoint,
}

/// Maps the client's contact ids onto host slots `0..MAX_TOUCH` and diffs successive frames.
///
/// A slot freed in an update is never reused in the same update, so a lift and a new finger
/// never share a slot (pointer id) inside one injection.
#[derive(Clone, Debug, Default)]
pub struct TouchTracker {
	slots: [Option<TouchPoint>; MAX_TOUCH],
}

impl TouchTracker {
	pub fn update(&mut self, next: &TouchFrame) -> Vec<SlotChange> {
		let mut out = Vec::new();
		let was_free: Vec<bool> = self.slots.iter().map(|s| s.is_none()).collect();
		for (slot, s) in self.slots.iter_mut().enumerate() {
			let Some(cur) = *s else { continue };
			match next.get(cur.id) {
				Some(p) => {
					*s = Some(*p);
					out.push(SlotChange { slot, phase: Contact::Move, pt: *p });
				}
				None => {
					*s = None;
					out.push(SlotChange { slot, phase: Contact::Up, pt: cur });
				}
			}
		}
		for p in next.points() {
			if self.slots.iter().flatten().any(|s| s.id == p.id) {
				continue;
			}
			let Some(slot) = (0..MAX_TOUCH).find(|&i| was_free[i] && self.slots[i].is_none()) else {
				break; // every slot that was free before this update is taken: ignore the finger
			};
			self.slots[slot] = Some(*p);
			out.push(SlotChange { slot, phase: Contact::Down, pt: *p });
		}
		out.sort_by_key(|c| c.slot);
		out
	}

	/// Lift every contact.
	pub fn release_all(&mut self) -> Vec<SlotChange> {
		self.update(&TouchFrame::default())
	}

	/// Live contacts as `(slot, point)`.
	pub fn active(&self) -> impl Iterator<Item = (usize, TouchPoint)> + '_ {
		self.slots.iter().enumerate().filter_map(|(i, s)| s.map(|p| (i, p)))
	}
}

/// A platform's pen / touch injector, driven by [`Driver`] on the worker thread.
pub trait Sink {
	/// Where normalized coordinates land (the streamed monitor), per platform.
	type Geom: Copy + Send + 'static;
	fn set_geom(&mut self, g: Self::Geom);
	/// Apply the pen moving from `prev` to `next` (both sanitized; `next` may equal `prev`
	/// apart from position).
	fn pen(&mut self, prev: &PenState, next: &PenState);
	/// Apply one touch update: `frame` is the new set of fingers, `changes` the per-slot edges.
	fn touch(&mut self, now: Instant, frame: &TouchFrame, changes: &[SlotChange]);
	/// Keep-alive re-injection of the unchanged state (Windows cancels injected pointers that
	/// are not refreshed). Called every [`Sink::refresh_every`] while something is active.
	fn refresh(&mut self, _pen: &PenState, _touch: &[(usize, TouchPoint)]) {}
	fn refresh_every(&self) -> Option<Duration> {
		None
	}
	/// Timers of the sink itself (macOS touch-as-mouse); returns when to call again.
	fn tick(&mut self, _now: Instant) -> Option<Instant> {
		None
	}
}

/// The host-side state machine shared by every platform: sanitizes, diffs, lifts on silence
/// and schedules the keep-alive. Pure (time is passed in), so it is unit-tested with a
/// recording sink.
pub struct Driver<S: Sink> {
	sink: S,
	pen: PenState,
	pen_at: Instant,
	tracker: TouchTracker,
	touch: TouchFrame,
	touch_at: Instant,
	refreshed_at: Instant,
}

impl<S: Sink> Driver<S> {
	pub fn new(sink: S, now: Instant) -> Self {
		Self {
			sink,
			pen: PenState::default(),
			pen_at: now,
			tracker: TouchTracker::default(),
			touch: TouchFrame::default(),
			touch_at: now,
			refreshed_at: now,
		}
	}

	pub fn sink(&self) -> &S {
		&self.sink
	}

	pub fn set_geom(&mut self, g: S::Geom) {
		self.sink.set_geom(g);
	}

	pub fn pen(&mut self, now: Instant, next: PenState) {
		let next = next.sanitized();
		if !next.in_range && !self.pen.in_range {
			return;
		}
		self.sink.pen(&self.pen, &next);
		self.pen = next;
		self.pen_at = now;
		self.refreshed_at = now;
	}

	pub fn touch(&mut self, now: Instant, frame: TouchFrame) {
		let frame = TouchFrame::new(frame.points());
		let changes = self.tracker.update(&frame);
		self.touch_at = now;
		if changes.is_empty() {
			return;
		}
		self.sink.touch(now, &frame, &changes);
		self.touch = frame;
		self.refreshed_at = now;
	}

	/// Lift the pen and every finger (control revoked, session over).
	pub fn release(&mut self, now: Instant) {
		if self.pen.in_range {
			self.pen(now, PenState { x: self.pen.x, y: self.pen.y, ..PenState::default() });
		}
		if !self.touch.is_empty() {
			self.touch(now, TouchFrame::default());
		}
	}

	fn active(&self) -> bool {
		self.pen.in_range || !self.touch.is_empty()
	}

	/// Run the timers; returns when to call again (`None` = nothing pending).
	pub fn tick(&mut self, now: Instant) -> Option<Instant> {
		if self.pen.in_range && now.duration_since(self.pen_at) >= SILENCE {
			let out = PenState { x: self.pen.x, y: self.pen.y, ..PenState::default() };
			self.sink.pen(&self.pen, &out);
			self.pen = out;
		}
		if !self.touch.is_empty() && now.duration_since(self.touch_at) >= SILENCE {
			let changes = self.tracker.release_all();
			self.touch = TouchFrame::default();
			self.sink.touch(now, &self.touch, &changes);
		}
		let mut next: Option<Instant> = self.sink.tick(now);
		let mut want = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
		if self.active() {
			if let Some(every) = self.sink.refresh_every() {
				if now.duration_since(self.refreshed_at) >= every {
					let active: Vec<_> = self.tracker.active().collect();
					self.sink.refresh(&self.pen, &active);
					self.refreshed_at = now;
				}
				want(self.refreshed_at + every);
			}
			if self.pen.in_range {
				want(self.pen_at + SILENCE);
			}
			if !self.touch.is_empty() {
				want(self.touch_at + SILENCE);
			}
		}
		next
	}
}

enum Cmd<G> {
	Pen(PenState),
	Touch(TouchFrame),
	Geom(G),
	Release,
}

/// [`Driver`] on its own thread. Cheap to hold (a channel sender, `Send`); the sink is built
/// on the worker (so a platform handle that is not `Send` stays there). Dropping it lifts
/// everything and ends the thread.
pub struct PenTouch<G> {
	tx: std::sync::mpsc::Sender<Cmd<G>>,
}

impl<G: Copy + Send + 'static> PenTouch<G> {
	/// Start the worker. `make` builds the sink; `None` (device creation failed) leaves a
	/// worker that only drains its queue (logged once).
	pub fn spawn<S, F>(name: &str, make: F) -> Self
	where
		S: Sink<Geom = G>,
		F: FnOnce() -> Option<S> + Send + 'static,
	{
		let (tx, rx) = std::sync::mpsc::channel::<Cmd<G>>();
		let label = name.to_string();
		let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
			let Some(sink) = make() else {
				tracing::warn!(worker = %label, "pen/touch injection unavailable — events ignored");
				for _ in rx {}
				return;
			};
			run(Driver::new(sink, Instant::now()), rx);
		});
		Self { tx }
	}

	pub fn pen(&self, s: PenState) {
		let _ = self.tx.send(Cmd::Pen(s));
	}
	pub fn touch(&self, f: TouchFrame) {
		let _ = self.tx.send(Cmd::Touch(f));
	}
	pub fn set_geom(&self, g: G) {
		let _ = self.tx.send(Cmd::Geom(g));
	}
	pub fn release(&self) {
		let _ = self.tx.send(Cmd::Release);
	}
}

fn run<S: Sink>(mut d: Driver<S>, rx: std::sync::mpsc::Receiver<Cmd<S::Geom>>) {
	use std::sync::mpsc::RecvTimeoutError;
	let mut deadline: Option<Instant> = None;
	loop {
		let cmd = match deadline {
			Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
			None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
		};
		let now = Instant::now();
		match cmd {
			Ok(Cmd::Pen(s)) => d.pen(now, s),
			Ok(Cmd::Touch(f)) => d.touch(now, f),
			Ok(Cmd::Geom(g)) => d.set_geom(g),
			Ok(Cmd::Release) => d.release(now),
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => {
				d.release(now);
				return;
			}
		}
		deadline = d.tick(Instant::now());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tp(id: u32, x: f32, y: f32) -> TouchPoint {
		TouchPoint { id, x, y, pressure: 0.0 }
	}

	#[test]
	fn touch_frame_json_is_a_plain_array_and_drops_duplicates() {
		let f = TouchFrame::new(&[tp(7, 0.25, 0.5), tp(7, 0.9, 0.9), tp(3, 2.0, -1.0)]);
		assert_eq!(f.len(), 2);
		assert_eq!(f.points()[1], tp(3, 1.0, 0.0));
		let json = serde_json::to_string(&f).unwrap();
		assert!(json.starts_with('['), "{json}");
		let back: TouchFrame = serde_json::from_str(&json).unwrap();
		assert_eq!(back, f);
		let many: Vec<TouchPoint> = (0..20).map(|i| tp(i, 0.1, 0.1)).collect();
		let big: TouchFrame = serde_json::from_str(&serde_json::to_string(&many).unwrap()).unwrap();
		assert_eq!(big.len(), MAX_TOUCH);
	}

	#[test]
	fn pen_state_tolerates_missing_fields_and_garbage() {
		let s: PenState = serde_json::from_str(r#"{"x":0.5,"y":0.25}"#).unwrap();
		assert!(!s.in_range && !s.contact);
		let s = PenState {
			x: f32::NAN,
			y: 3.0,
			pressure: 9.0,
			tilt_x: -200.0,
			twist: -10.0,
			in_range: true,
			contact: true,
			..Default::default()
		}
		.sanitized();
		assert_eq!((s.x, s.y, s.pressure, s.tilt_x, s.twist), (0.0, 1.0, 1.0, -90.0, 350.0));
		// Hovering carries no pressure; out of range carries nothing but the position.
		let h = PenState { in_range: true, pressure: 0.7, ..Default::default() }.sanitized();
		assert_eq!(h.pressure, 0.0);
		let o = PenState { contact: true, barrel: true, x: 0.3, ..Default::default() }.sanitized();
		assert_eq!(o, PenState { x: 0.3, ..Default::default() });
	}

	#[test]
	fn tracker_assigns_stable_slots_and_never_reuses_a_freed_slot_in_one_update() {
		let mut t = TouchTracker::default();
		let c = t.update(&TouchFrame::new(&[tp(100, 0.1, 0.1), tp(200, 0.2, 0.2)]));
		assert_eq!(
			c.iter().map(|c| (c.slot, c.phase)).collect::<Vec<_>>(),
			vec![(0, Contact::Down), (1, Contact::Down)]
		);
		// 100 lifts while 300 lands in the same frame: 300 must not take slot 0 yet.
		let c = t.update(&TouchFrame::new(&[tp(200, 0.3, 0.3), tp(300, 0.4, 0.4)]));
		assert_eq!(
			c.iter().map(|c| (c.slot, c.phase, c.pt.id)).collect::<Vec<_>>(),
			vec![(0, Contact::Up, 100), (1, Contact::Move, 200), (2, Contact::Down, 300)]
		);
		assert_eq!(c[0].pt, tp(100, 0.1, 0.1), "an Up carries the last known position");
		// Next frame: a new finger may take slot 0 again.
		let c = t.update(&TouchFrame::new(&[tp(200, 0.3, 0.3), tp(300, 0.4, 0.4), tp(400, 0.5, 0.5)]));
		assert!(c.contains(&SlotChange { slot: 0, phase: Contact::Down, pt: tp(400, 0.5, 0.5) }));
		let up = t.release_all();
		assert_eq!(up.len(), 3);
		assert!(up.iter().all(|c| c.phase == Contact::Up));
		assert_eq!(t.active().count(), 0);
	}

	#[derive(Default)]
	struct Rec {
		pens: Vec<(PenState, PenState)>,
		touches: Vec<Vec<SlotChange>>,
		refreshes: usize,
		every: Option<Duration>,
	}
	impl Sink for Rec {
		type Geom = ();
		fn set_geom(&mut self, _: ()) {}
		fn pen(&mut self, prev: &PenState, next: &PenState) {
			self.pens.push((*prev, *next));
		}
		fn touch(&mut self, _: Instant, _: &TouchFrame, changes: &[SlotChange]) {
			self.touches.push(changes.to_vec());
		}
		fn refresh(&mut self, _: &PenState, _: &[(usize, TouchPoint)]) {
			self.refreshes += 1;
		}
		fn refresh_every(&self) -> Option<Duration> {
			self.every
		}
	}

	#[test]
	fn driver_lifts_a_silent_pen_and_touch_and_refreshes_while_active() {
		let t0 = Instant::now();
		let ms = |m: u64| t0 + Duration::from_millis(m);
		let mut d = Driver::new(Rec { every: Some(Duration::from_millis(50)), ..Default::default() }, t0);
		// Out-of-range noise is not forwarded.
		d.pen(t0, PenState::default());
		assert!(d.sink().pens.is_empty());
		d.pen(t0, PenState { in_range: true, contact: true, pressure: 0.5, x: 0.5, y: 0.5, ..Default::default() });
		d.touch(t0, TouchFrame::new(&[tp(1, 0.5, 0.5)]));
		assert_eq!(d.tick(ms(10)), Some(ms(50)));
		d.tick(ms(60));
		assert_eq!(d.sink().refreshes, 1);
		// Silence: both lifted at 1 s.
		assert!(d.tick(ms(1000)).is_none());
		let (_, last) = *d.sink().pens.last().unwrap();
		assert!(!last.in_range && !last.contact);
		assert_eq!(d.sink().touches.last().unwrap()[0].phase, Contact::Up);
		// Nothing active → no more timers, no refresh.
		let r = d.sink().refreshes;
		assert!(d.tick(ms(5000)).is_none());
		assert_eq!(d.sink().refreshes, r);
	}

	#[test]
	fn driver_release_lifts_everything_once() {
		let t0 = Instant::now();
		let mut d = Driver::new(Rec::default(), t0);
		d.pen(t0, PenState { in_range: true, ..Default::default() });
		d.touch(t0, TouchFrame::new(&[tp(1, 0.1, 0.1), tp(2, 0.2, 0.2)]));
		d.release(t0);
		d.release(t0);
		assert_eq!(d.sink().pens.len(), 2);
		assert_eq!(d.sink().touches.len(), 2);
		assert_eq!(d.sink().touches[1].len(), 2);
		// A repeated identical frame of a still finger is forwarded (Windows needs every
		// active contact refreshed), an empty one after all lifted is not.
		d.touch(t0, TouchFrame::default());
		assert_eq!(d.sink().touches.len(), 2);
	}
}
