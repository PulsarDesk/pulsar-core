//! A USB/IP 1.1.1 server subset: one exported device, one session at a time, bound to
//! 127.0.0.1. Enough for `usbip attach` (usbip-win2 on Windows, `vhci-hcd` on Linux) to
//! plug the emulated device into the local USB stack.
//!
//! Ported from ZerkSense5 (MIT, florinp93), `Core/UsbIpServer.cs`. Protocol notes:
//! `ds5-protocol.md` §6.2. All integers on the wire are big-endian.
//!
//! - `OP_REQ_DEVLIST` / `OP_REQ_IMPORT`, then `CMD_SUBMIT` / `RET_SUBMIT` and
//!   `CMD_UNLINK` / `RET_UNLINK` on the imported socket.
//! - Every URB carries an atomic "claimed" flag: whichever of completion and unlink
//!   claims it first answers it, so an unlinked URB never gets a `RET_SUBMIT`.
//! - Control transfers run on their own thread (in order).
//! - Isochronous URBs are paced by [`IsoPacer`]: each stream keeps a 1 ms clock, so the
//!   host sees a steady 1 kHz audio clock.
//! - Interrupt IN coalesces: with no URB pending only the newest report is kept.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The only bus id this server exports.
pub const BUS_ID: &str = "1-1";
const VERSION: u16 = 0x0111;
const BUS_NUM: u32 = 1;
const DEV_NUM: u32 = 1;
/// USB full speed.
const SPEED_FULL: u32 = 2;

const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;

pub const CMD_SUBMIT: u32 = 1;
pub const CMD_UNLINK: u32 = 2;
pub const RET_SUBMIT: u32 = 3;
pub const RET_UNLINK: u32 = 4;
pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// Broken pipe: a STALL.
pub const EPIPE: i32 = 32;
/// Connection reset: the status of an unlinked URB.
pub const ECONNRESET: i32 = 104;

/// Size of a device record in DEVLIST / IMPORT replies.
pub const DEVICE_RECORD_LEN: usize = 312;
/// Size of every URB header.
pub const URB_HEADER_LEN: usize = 48;

/// The emulated device behind the server (`IUsbDevice` in ZerkSense).
pub trait UsbDevice: Send + Sync {
	fn vendor_id(&self) -> u16;
	fn product_id(&self) -> u16;
	fn bcd_device(&self) -> u16;
	/// `(class, subclass, protocol)` of every interface, in order.
	fn interfaces(&self) -> &[(u8, u8, u8)];
	/// An EP0 request. `Ok` carries the IN data (empty for OUT); `Err` a negative errno.
	fn control(&self, setup: &[u8; 8], out: &[u8]) -> Result<Vec<u8>, i32>;
	fn interrupt_out(&self, ep: u32, data: &[u8]);
	/// All packets of one isochronous OUT URB, in order.
	fn iso_out(&self, ep: u32, packets: &[&[u8]]);
	/// Fill one isochronous IN packet.
	fn iso_in(&self, ep: u32, packet: &mut [u8]);
	fn is_isochronous(&self, ep: u32) -> bool;
	fn iso_in_packet_size(&self, ep: u32) -> usize;
	/// A session started (`true`) or ended (`false`).
	fn session_changed(&self, attached: bool);
}

fn be16(b: &[u8], o: usize) -> u16 {
	u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
	u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn put32(b: &mut [u8], o: usize, v: u32) {
	b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

/// The 8-byte OP header.
pub fn op_header(code: u16, status: u32) -> [u8; 8] {
	let mut b = [0u8; 8];
	b[0..2].copy_from_slice(&VERSION.to_be_bytes());
	b[2..4].copy_from_slice(&code.to_be_bytes());
	put32(&mut b, 4, status);
	b
}

/// The 312-byte device record.
pub fn device_record(dev: &dyn UsbDevice) -> [u8; DEVICE_RECORD_LEN] {
	let mut b = [0u8; DEVICE_RECORD_LEN];
	let path = b"/sys/devices/pci0000:00/0000:00:1d.0/usb1/1-1";
	b[..path.len()].copy_from_slice(path);
	b[256..256 + BUS_ID.len()].copy_from_slice(BUS_ID.as_bytes());
	put32(&mut b, 288, BUS_NUM);
	put32(&mut b, 292, DEV_NUM);
	put32(&mut b, 296, SPEED_FULL);
	b[300..302].copy_from_slice(&dev.vendor_id().to_be_bytes());
	b[302..304].copy_from_slice(&dev.product_id().to_be_bytes());
	b[304..306].copy_from_slice(&dev.bcd_device().to_be_bytes());
	// 306..309: device class / subclass / protocol = 0 (per interface).
	b[309] = 1; // bConfigurationValue
	b[310] = 1; // bNumConfigurations
	b[311] = dev.interfaces().len() as u8;
	b
}

/// A running USB/IP server. Dropping it closes the session and stops every thread.
pub struct UsbIpServer {
	shared: Arc<ServerShared>,
	port: u16,
	accept: Option<JoinHandle<()>>,
}

struct ServerShared {
	device: Arc<dyn UsbDevice>,
	session: Mutex<Option<Arc<Session>>>,
	stop: AtomicBool,
}

impl UsbIpServer {
	/// Bind 127.0.0.1:`port` (0 = ephemeral) and start accepting.
	pub fn start(port: u16, device: Arc<dyn UsbDevice>) -> io::Result<Self> {
		let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
		let port = listener.local_addr()?.port();
		let shared = Arc::new(ServerShared {
			device,
			session: Mutex::new(None),
			stop: AtomicBool::new(false),
		});
		let s = shared.clone();
		let accept = thread::Builder::new()
			.name("usbip-accept".into())
			.spawn(move || accept_loop(listener, s))?;
		Ok(Self {
			shared,
			port,
			accept: Some(accept),
		})
	}

	/// The bound TCP port.
	pub fn port(&self) -> u16 {
		self.port
	}

	/// True while a client has the device imported.
	pub fn attached(&self) -> bool {
		self.shared.session.lock().unwrap().is_some()
	}

	/// Complete the oldest pending interrupt IN URB on `ep` with `report`, or keep it as
	/// the latest report when none is pending. Dropped when no session is open.
	pub fn push_interrupt_in(&self, ep: u32, report: &[u8]) {
		let session = self.shared.session.lock().unwrap().clone();
		if let Some(s) = session {
			s.push_interrupt_in(ep, report);
		}
	}
}

impl Drop for UsbIpServer {
	fn drop(&mut self) {
		self.shared.stop.store(true, Ordering::SeqCst);
		// Wake the blocking accept().
		let _ = TcpStream::connect_timeout(
			&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
			Duration::from_millis(200),
		);
		if let Some(s) = self.shared.session.lock().unwrap().take() {
			s.close();
		}
		if let Some(h) = self.accept.take() {
			let _ = h.join();
		}
	}
}

fn accept_loop(listener: TcpListener, shared: Arc<ServerShared>) {
	for conn in listener.incoming() {
		if shared.stop.load(Ordering::SeqCst) {
			return;
		}
		let Ok(stream) = conn else { continue };
		let _ = stream.set_nodelay(true);
		let s = shared.clone();
		let _ = thread::Builder::new()
			.name("usbip-client".into())
			.spawn(move || {
				if let Err(e) = handle_client(stream, &s) {
					tracing::debug!("usbip: client ended: {e}");
				}
			});
	}
}

fn handle_client(mut stream: TcpStream, shared: &Arc<ServerShared>) -> io::Result<()> {
	let mut hdr = [0u8; 8];
	stream.read_exact(&mut hdr)?;
	let dev = &shared.device;
	match be16(&hdr, 2) {
		OP_REQ_DEVLIST => {
			let mut reply = op_header(OP_REP_DEVLIST, 0).to_vec();
			reply.extend_from_slice(&1u32.to_be_bytes());
			reply.extend_from_slice(&device_record(dev.as_ref()));
			for &(c, s, p) in dev.interfaces() {
				reply.extend_from_slice(&[c, s, p, 0]);
			}
			stream.write_all(&reply)?;
			Ok(())
		}
		OP_REQ_IMPORT => {
			let mut bus = [0u8; 32];
			stream.read_exact(&mut bus)?;
			let end = bus.iter().position(|&b| b == 0).unwrap_or(32);
			let requested = &bus[..end];
			let session = {
				let mut slot = shared.session.lock().unwrap();
				if requested != BUS_ID.as_bytes()
					|| slot.is_some()
					|| shared.stop.load(Ordering::SeqCst)
				{
					stream.write_all(&op_header(OP_REP_IMPORT, 1))?;
					return Ok(());
				}
				let mut reply = op_header(OP_REP_IMPORT, 0).to_vec();
				reply.extend_from_slice(&device_record(dev.as_ref()));
				stream.write_all(&reply)?;
				let s = Session::new(&stream, dev.clone())?;
				*slot = Some(s.clone());
				s
			};
			tracing::info!("usbip: virtual DualSense attached");
			dev.session_changed(true);
			session.run(stream);
			{
				let mut slot = shared.session.lock().unwrap();
				if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &session)) {
					*slot = None;
				}
			}
			session.close();
			dev.session_changed(false);
			tracing::info!("usbip: virtual DualSense detached");
			Ok(())
		}
		_ => Ok(()),
	}
}

/// One URB in flight.
struct Urb {
	seq: u32,
	ep: u32,
	dir: u32,
	len: u32,
	/// Number of iso packets, or `None` for non-iso transfers.
	packets: Option<u32>,
	setup: [u8; 8],
	data: Vec<u8>,
	/// `(offset, length)` per iso packet.
	iso: Vec<(u32, u32)>,
	claimed: AtomicBool,
}

impl Urb {
	/// True for exactly one caller: completion or unlink.
	fn claim(&self) -> bool {
		!self.claimed.swap(true, Ordering::SeqCst)
	}
}

#[derive(Default)]
struct InterruptIn {
	pending: HashMap<u32, VecDeque<Arc<Urb>>>,
	latest: HashMap<u32, Vec<u8>>,
}

struct Session {
	me: Weak<Session>,
	writer: Mutex<TcpStream>,
	device: Arc<dyn UsbDevice>,
	pending: Mutex<HashMap<u32, Arc<Urb>>>,
	interrupt: Mutex<InterruptIn>,
	pacer: IsoPacer,
	control_tx: Mutex<Option<mpsc::Sender<Arc<Urb>>>>,
	closed: AtomicBool,
}

impl Session {
	fn new(stream: &TcpStream, device: Arc<dyn UsbDevice>) -> io::Result<Arc<Self>> {
		let writer = stream.try_clone()?;
		let (tx, rx) = mpsc::channel::<Arc<Urb>>();
		let s = Arc::new_cyclic(|me| Self {
			me: me.clone(),
			writer: Mutex::new(writer),
			device,
			pending: Mutex::new(HashMap::new()),
			interrupt: Mutex::new(InterruptIn::default()),
			pacer: IsoPacer::start(),
			control_tx: Mutex::new(Some(tx)),
			closed: AtomicBool::new(false),
		});
		let weak = Arc::downgrade(&s);
		thread::Builder::new()
			.name("usbip-control".into())
			.spawn(move || {
				for urb in rx {
					let Some(s) = weak.upgrade() else { return };
					s.complete_control(&urb);
				}
			})?;
		Ok(s)
	}

	fn run(&self, mut stream: TcpStream) {
		let mut hdr = [0u8; URB_HEADER_LEN];
		while !self.closed.load(Ordering::SeqCst) {
			if stream.read_exact(&mut hdr).is_err() {
				break;
			}
			let r = match be32(&hdr, 0) {
				CMD_SUBMIT => self.read_submit(&hdr, &mut stream),
				CMD_UNLINK => {
					self.unlink(be32(&hdr, 4), be32(&hdr, 20));
					Ok(())
				}
				_ => break,
			};
			if r.is_err() {
				break;
			}
		}
	}

	fn close(&self) {
		if self.closed.swap(true, Ordering::SeqCst) {
			return;
		}
		self.pacer.stop();
		self.control_tx.lock().unwrap().take();
		let _ = self.writer.lock().unwrap().shutdown(Shutdown::Both);
	}

	fn read_submit(&self, hdr: &[u8; URB_HEADER_LEN], stream: &mut TcpStream) -> io::Result<()> {
		let ep = be32(hdr, 16);
		let dir = be32(hdr, 12);
		let len = be32(hdr, 24);
		let n = be32(hdr, 32);
		let iso = self.device.is_isochronous(ep) && n != u32::MAX && n > 0;
		let mut data = Vec::new();
		if dir == DIR_OUT && len > 0 {
			data = vec![0u8; len as usize];
			stream.read_exact(&mut data)?;
		}
		let mut iso_desc = Vec::new();
		if iso {
			let mut d = vec![0u8; 16 * n as usize];
			stream.read_exact(&mut d)?;
			iso_desc = d
				.chunks_exact(16)
				.map(|c| (be32(c, 0), be32(c, 4)))
				.collect();
		}
		let urb = Arc::new(Urb {
			seq: be32(hdr, 4),
			ep,
			dir,
			len,
			packets: iso.then_some(n),
			setup: hdr[40..48].try_into().unwrap(),
			data,
			iso: iso_desc,
			claimed: AtomicBool::new(false),
		});
		self.pending.lock().unwrap().insert(urb.seq, urb.clone());

		if ep == 0 {
			if let Some(tx) = self.control_tx.lock().unwrap().as_ref() {
				let _ = tx.send(urb);
			}
		} else if let Some(packets) = urb.packets {
			let stream_id = ep + if dir == DIR_IN { 16 } else { 0 };
			let weak = self.me.clone();
			self.pacer.schedule(
				stream_id,
				packets,
				Box::new(move || {
					if let Some(s) = weak.upgrade() {
						s.complete_iso(&urb);
					}
				}),
			);
		} else if dir == DIR_OUT {
			self.device.interrupt_out(ep, &urb.data);
			if urb.claim() {
				self.send_submit(&urb, 0, urb.len, &[], None);
			}
		} else {
			let ready = {
				let mut i = self.interrupt.lock().unwrap();
				match i.latest.remove(&ep) {
					Some(r) => Some(r),
					None => {
						i.pending.entry(ep).or_default().push_back(urb.clone());
						None
					}
				}
			};
			if let Some(r) = ready {
				if urb.claim() {
					self.send_interrupt_in(&urb, &r);
				}
			}
		}
		Ok(())
	}

	fn complete_control(&self, urb: &Urb) {
		let out = if urb.dir == DIR_OUT {
			&urb.data[..]
		} else {
			&[]
		};
		let result = self.device.control(&urb.setup, out);
		if !urb.claim() {
			return;
		}
		match result {
			Ok(mut data) if urb.dir == DIR_IN => {
				data.truncate(urb.len as usize);
				self.send_submit(urb, 0, data.len() as u32, &data, None);
			}
			Ok(_) => self.send_submit(urb, 0, urb.len, &[], None),
			Err(e) => self.send_submit(urb, e, 0, &[], None),
		}
	}

	fn complete_iso(&self, urb: &Urb) {
		if !urb.claim() {
			return;
		}
		let mut actual = Vec::with_capacity(urb.iso.len());
		if urb.dir == DIR_OUT {
			let mut total = 0u32;
			let mut slices = Vec::with_capacity(urb.iso.len());
			for &(off, len) in &urb.iso {
				let (o, l) = (off as usize, len as usize);
				if o + l <= urb.data.len() {
					slices.push(&urb.data[o..o + l]);
				}
				actual.push(len);
				total += len;
			}
			self.device.iso_out(urb.ep, &slices);
			self.send_submit(urb, 0, total, &[], Some(&actual));
		} else {
			let want = self.device.iso_in_packet_size(urb.ep);
			let mut packed = Vec::with_capacity(urb.iso.len() * want);
			for &(_, len) in &urb.iso {
				let n = want.min(len as usize);
				let start = packed.len();
				packed.resize(start + n, 0);
				self.device.iso_in(urb.ep, &mut packed[start..]);
				actual.push(n as u32);
			}
			self.send_submit(urb, 0, packed.len() as u32, &packed, Some(&actual));
		}
	}

	fn push_interrupt_in(&self, ep: u32, report: &[u8]) {
		let urb = {
			let mut i = self.interrupt.lock().unwrap();
			let mut found = None;
			if let Some(q) = i.pending.get_mut(&ep) {
				while let Some(u) = q.pop_front() {
					if u.claim() {
						found = Some(u);
						break;
					}
				}
			}
			if found.is_none() {
				i.latest.insert(ep, report.to_vec());
			}
			found
		};
		if let Some(u) = urb {
			self.send_interrupt_in(&u, report);
		}
	}

	fn send_interrupt_in(&self, urb: &Urb, report: &[u8]) {
		let n = report.len().min(urb.len as usize);
		self.send_submit(urb, 0, n as u32, &report[..n], None);
	}

	fn unlink(&self, seq: u32, victim: u32) {
		let urb = self.pending.lock().unwrap().remove(&victim);
		let status = match urb {
			Some(u) if u.claim() => -ECONNRESET,
			_ => 0,
		};
		let mut b = [0u8; URB_HEADER_LEN];
		put32(&mut b, 0, RET_UNLINK);
		put32(&mut b, 4, seq);
		put32(&mut b, 20, status as u32);
		self.send(&b);
	}

	fn send_submit(&self, urb: &Urb, status: i32, actual: u32, data: &[u8], iso: Option<&[u32]>) {
		self.pending.lock().unwrap().remove(&urb.seq);
		let np = urb.packets.unwrap_or(0) as usize;
		let mut b = vec![0u8; URB_HEADER_LEN + data.len() + 16 * np];
		put32(&mut b, 0, RET_SUBMIT);
		put32(&mut b, 4, urb.seq);
		put32(&mut b, 20, status as u32);
		put32(&mut b, 24, actual);
		put32(
			&mut b,
			28,
			if urb.packets.is_some() {
				self.pacer.frame_number()
			} else {
				0
			},
		);
		put32(&mut b, 32, urb.packets.unwrap_or(u32::MAX));
		b[URB_HEADER_LEN..URB_HEADER_LEN + data.len()].copy_from_slice(data);
		if let Some(actual) = iso {
			let mut p = URB_HEADER_LEN + data.len();
			for (i, &(off, len)) in urb.iso.iter().enumerate() {
				put32(&mut b, p, off);
				put32(&mut b, p + 4, len);
				put32(&mut b, p + 8, actual[i]);
				p += 16;
			}
		}
		self.send(&b);
	}

	fn send(&self, b: &[u8]) {
		if self.closed.load(Ordering::SeqCst) {
			return;
		}
		let r = self.writer.lock().unwrap().write_all(b);
		if r.is_err() {
			self.close();
		}
	}
}

type Job = Box<dyn FnOnce() + Send>;

struct Entry {
	due: Instant,
	id: u64,
	job: Job,
}

impl PartialEq for Entry {
	fn eq(&self, o: &Self) -> bool {
		(self.due, self.id) == (o.due, o.id)
	}
}
impl Eq for Entry {}
impl PartialOrd for Entry {
	fn partial_cmp(&self, o: &Self) -> Option<CmpOrdering> {
		Some(self.cmp(o))
	}
}
impl Ord for Entry {
	/// Reversed, so the `BinaryHeap` pops the earliest deadline first.
	fn cmp(&self, o: &Self) -> CmpOrdering {
		(o.due, o.id).cmp(&(self.due, self.id))
	}
}

#[derive(Default)]
struct PacerState {
	queue: BinaryHeap<Entry>,
	clocks: HashMap<u32, Instant>,
	next_id: u64,
	stop: bool,
}

struct PacerShared {
	state: Mutex<PacerState>,
	wake: Condvar,
	epoch: Instant,
}

/// Completes isochronous URBs on a per-stream 1 ms clock: a URB of N packets is due
/// N ms after the previous one on its stream. A stream that falls more than 20 ms
/// behind restarts from now.
pub struct IsoPacer {
	shared: Arc<PacerShared>,
}

const MS: Duration = Duration::from_millis(1);
const RESYNC: Duration = Duration::from_millis(20);

impl IsoPacer {
	pub fn start() -> Self {
		let shared = Arc::new(PacerShared {
			state: Mutex::new(PacerState::default()),
			wake: Condvar::new(),
			epoch: Instant::now(),
		});
		let s = shared.clone();
		let _ = thread::Builder::new()
			.name("usbip-iso-pacer".into())
			.spawn(move || pacer_loop(&s));
		Self { shared }
	}

	/// USB frame number: milliseconds since start, 11 bits.
	pub fn frame_number(&self) -> u32 {
		(self.shared.epoch.elapsed().as_millis() as u32) & 0x7FF
	}

	/// Run `job` when `packets` more milliseconds of `stream` have elapsed.
	pub fn schedule(&self, stream: u32, packets: u32, job: Job) {
		let now = Instant::now();
		let mut st = self.shared.state.lock().unwrap();
		let mut next = st.clocks.get(&stream).copied().unwrap_or(now);
		if next + RESYNC < now {
			next = now;
		}
		let due = next + MS * packets;
		st.clocks.insert(stream, due);
		let id = st.next_id;
		st.next_id += 1;
		st.queue.push(Entry { due, id, job });
		drop(st);
		self.shared.wake.notify_one();
	}

	pub fn stop(&self) {
		let mut st = self.shared.state.lock().unwrap();
		st.stop = true;
		st.queue.clear();
		drop(st);
		self.shared.wake.notify_one();
	}
}

impl Drop for IsoPacer {
	fn drop(&mut self) {
		self.stop();
	}
}

fn pacer_loop(s: &PacerShared) {
	let _timer = timer::HighRes::begin();
	let mut st = s.state.lock().unwrap();
	loop {
		if st.stop {
			return;
		}
		let now = Instant::now();
		let wait = match st.queue.peek() {
			Some(e) if e.due <= now => {
				let job = st.queue.pop().unwrap().job;
				drop(st);
				job();
				st = s.state.lock().unwrap();
				continue;
			}
			Some(e) => Some(e.due - now),
			None => None,
		};
		match wait {
			None => {
				st = s
					.wake
					.wait_timeout(st, Duration::from_millis(100))
					.unwrap()
					.0
			}
			// Sleep to ~1 ms before the deadline, then spin (ZerkSense's hybrid wait).
			Some(w) if w > MS + MS / 2 => st = s.wake.wait_timeout(st, w - MS).unwrap().0,
			Some(_) => {
				drop(st);
				thread::yield_now();
				st = s.state.lock().unwrap();
			}
		}
	}
}

/// 1 ms system timer resolution and a raised thread priority while the pacer runs.
mod timer {
	pub struct HighRes;

	#[cfg(windows)]
	impl HighRes {
		pub fn begin() -> Self {
			use windows::Win32::System::Threading::{
				GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
			};
			unsafe {
				windows::Win32::Media::timeBeginPeriod(1);
				let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
			}
			Self
		}
	}

	#[cfg(windows)]
	impl Drop for HighRes {
		fn drop(&mut self) {
			unsafe {
				windows::Win32::Media::timeEndPeriod(1);
			}
		}
	}

	#[cfg(not(windows))]
	impl HighRes {
		pub fn begin() -> Self {
			Self
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pacer_spaces_urbs_by_packet_count() {
		let p = IsoPacer::start();
		let (tx, rx) = mpsc::channel();
		let t0 = Instant::now();
		for _ in 0..3 {
			let tx = tx.clone();
			p.schedule(1, 8, Box::new(move || tx.send(Instant::now()).unwrap()));
		}
		let times: Vec<_> = (0..3)
			.map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap() - t0)
			.collect();
		// Due at 8, 16, 24 ms; allow generous scheduler slack on CI.
		assert!(times[0] >= Duration::from_millis(8), "{times:?}");
		assert!(times[2] >= Duration::from_millis(24), "{times:?}");
		assert!(times[2] < Duration::from_millis(500), "{times:?}");
	}
}
