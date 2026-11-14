//! macOS host-side virtual gamepad: a virtual HID device created with
//! `IOHIDUserDeviceCreateWithProperties` (IOKit, macOS 10.15+).
//!
//! The device impersonates a pad macOS already drives, so games see a real controller
//! through every API they use — GameController (`GCController`), IOHIDManager and SDL's
//! HIDAPI driver:
//! * [`ResolvedTarget::Xbox360`] → an Xbox Wireless Controller over Bluetooth (045E:0B13);
//!   a wired Xbox 360 pad has no macOS driver (XUSB is not HID), the Bluetooth one is HID.
//! * [`ResolvedTarget::Ds4`] → a DualShock 4 v2 over USB (054C:09CC).
//!
//! Report layouts, descriptors and rumble decoding live in the platform-independent
//! [`super::virtual_hid`] (unit-tested on every host). Rumble: the game's output reports
//! (Xbox 0x03 / DS4 0x05) arrive in the set-report block on a private dispatch queue and are
//! forwarded through [`RumbleReader`], like the ViGEm DS4 and Duo backends on Windows.
//!
//! **Entitlement:** creating the device needs `com.apple.developer.hid.virtual.device` in
//! the app's code signature (Apple grants it with a provisioning profile). An unsigned,
//! ad-hoc-signed or self-built app is refused — `IOHIDUserDeviceCreateWithProperties`
//! returns NULL — and [`MacHidGamepad::new_target`] reports [`PadFailure::MacEntitlement`]
//! (checked with `SecTaskCopyValueForEntitlement`, so a refusal despite the entitlement is
//! reported as [`PadFailure::Other`]). The IOKit symbols are resolved with `dlsym`, so the
//! app still launches on macOS releases older than 10.15 ([`PadFailure::MacUnsupported`]).
//!
//! Untested on macOS hardware: written against the IOHIDUserDevice.h / IOHIDKeys.h
//! contract and the pads' documented report formats.

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

use block2::RcBlock;
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFMutableDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use super::types::{
	GamepadKind, GamepadState, PadFailure, ResolvedTarget, RumbleReader, VirtualGamepad,
};
use super::virtual_hid as hid;

/// `kIOReturnSuccess` / `kIOReturnUnsupported`.
const IO_RETURN_SUCCESS: i32 = 0;
const IO_RETURN_UNSUPPORTED: i32 = 0xE000_02C7_u32 as i32;
/// `IOHIDReportType`.
const REPORT_TYPE_INPUT: u32 = 0;
const REPORT_TYPE_FEATURE: u32 = 2;
const ENTITLEMENT: &str = "com.apple.developer.hid.virtual.device";

type CreateFn = unsafe extern "C" fn(*const c_void, *const c_void, u32) -> *mut c_void;
type DevBlockFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
type DevFn = unsafe extern "C" fn(*mut c_void);
type HandleReportFn = unsafe extern "C" fn(*mut c_void, u64, *const u8, isize) -> i32;

extern "C" {
	// libSystem (libdispatch + mach), always linked.
	fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
	fn dispatch_release(object: *mut c_void);
	fn mach_absolute_time() -> u64;
}

/// The IOHIDUserDevice entry points, resolved at runtime (macOS 10.15+).
struct Api {
	create: CreateFn,
	register_get: DevBlockFn,
	register_set: DevBlockFn,
	set_queue: DevBlockFn,
	set_cancel: DevBlockFn,
	activate: DevFn,
	cancel: DevFn,
	handle_report: HandleReportFn,
}

unsafe fn sym(lib: *mut c_void, name: &CStr) -> Option<*mut c_void> {
	let p = libc::dlsym(lib, name.as_ptr());
	(!p.is_null()).then_some(p)
}

fn api() -> Option<&'static Api> {
	static API: OnceLock<Option<Api>> = OnceLock::new();
	API.get_or_init(|| unsafe {
		let lib = libc::dlopen(
			c"/System/Library/Frameworks/IOKit.framework/IOKit".as_ptr(),
			libc::RTLD_LAZY,
		);
		if lib.is_null() {
			return None;
		}
		Some(Api {
			create: std::mem::transmute::<*mut c_void, CreateFn>(sym(
				lib,
				c"IOHIDUserDeviceCreateWithProperties",
			)?),
			register_get: std::mem::transmute::<*mut c_void, DevBlockFn>(sym(
				lib,
				c"IOHIDUserDeviceRegisterGetReportBlock",
			)?),
			register_set: std::mem::transmute::<*mut c_void, DevBlockFn>(sym(
				lib,
				c"IOHIDUserDeviceRegisterSetReportBlock",
			)?),
			set_queue: std::mem::transmute::<*mut c_void, DevBlockFn>(sym(
				lib,
				c"IOHIDUserDeviceSetDispatchQueue",
			)?),
			set_cancel: std::mem::transmute::<*mut c_void, DevBlockFn>(sym(
				lib,
				c"IOHIDUserDeviceSetCancelHandler",
			)?),
			activate: std::mem::transmute::<*mut c_void, DevFn>(sym(
				lib,
				c"IOHIDUserDeviceActivate",
			)?),
			cancel: std::mem::transmute::<*mut c_void, DevFn>(sym(lib, c"IOHIDUserDeviceCancel")?),
			handle_report: std::mem::transmute::<*mut c_void, HandleReportFn>(sym(
				lib,
				c"IOHIDUserDeviceHandleReportWithTimeStamp",
			)?),
		})
	})
	.as_ref()
}

/// Does this process's code signature carry the virtual-HID entitlement? `None` when the
/// Security framework can't say.
fn has_entitlement() -> Option<bool> {
	type CreateSelf = unsafe extern "C" fn(*const c_void) -> *mut c_void;
	type CopyValue = unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> CFTypeRef;
	unsafe {
		let lib = libc::dlopen(
			c"/System/Library/Frameworks/Security.framework/Security".as_ptr(),
			libc::RTLD_LAZY,
		);
		if lib.is_null() {
			return None;
		}
		let create: CreateSelf = std::mem::transmute(sym(lib, c"SecTaskCreateFromSelf")?);
		let copy: CopyValue = std::mem::transmute(sym(lib, c"SecTaskCopyValueForEntitlement")?);
		let task = create(std::ptr::null());
		if task.is_null() {
			return None;
		}
		let key = CFString::from_static_string(ENTITLEMENT);
		let value = copy(task, key.as_concrete_TypeRef() as *const c_void, std::ptr::null_mut());
		CFRelease(task as CFTypeRef);
		if value.is_null() {
			return Some(false);
		}
		let value = CFType::wrap_under_create_rule(value);
		Some(value.downcast::<CFBoolean>().map(bool::from).unwrap_or(true))
	}
}

/// Which pad the virtual device impersonates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Personality {
	Xbox,
	Ds4,
}

/// A live virtual HID gamepad. Dropping it cancels the device (the kernel unplugs it).
pub struct MacHidGamepad {
	kind: GamepadKind,
	personality: Personality,
	api: &'static Api,
	/// The `IOHIDUserDeviceRef`; released by the cancel handler after [`Drop`] cancels it.
	dev: *mut c_void,
	/// The latest input report (also answers a driver's GET_REPORT for it).
	last_input: Arc<Mutex<Vec<u8>>>,
	rumble_tx: Arc<Mutex<Option<mpsc::Sender<(u8, u8)>>>>,
	rumble_rx: Mutex<Option<mpsc::Receiver<(u8, u8)>>>,
	counter: u8,
	started: Instant,
	warned: bool,
}

// SAFETY: the IOHIDUserDeviceRef is a thread-safe CF object (reports may be posted from any
// thread; callbacks run on the device's own dispatch queue). Only `Drop` cancels it.
unsafe impl Send for MacHidGamepad {}

impl MacHidGamepad {
	pub fn new_target(kind: GamepadKind, resolved: ResolvedTarget) -> Result<Self, PadFailure> {
		let api = api().ok_or(PadFailure::MacUnsupported)?;
		let personality = match resolved {
			ResolvedTarget::Xbox360 => Personality::Xbox,
			ResolvedTarget::Ds4 => Personality::Ds4,
		};
		static NEXT: AtomicU32 = AtomicU32::new(1);
		let unique = (std::process::id() << 8) ^ NEXT.fetch_add(1, Ordering::Relaxed);

		let props = properties(personality, unique);
		// Options 0 (not CreateOnActivate): the kernel device — and the entitlement check —
		// happen right here, so a refusal is a NULL we can report instead of a silent no-op.
		let dev = unsafe {
			(api.create)(std::ptr::null(), props.as_concrete_TypeRef() as *const c_void, 0)
		};
		if dev.is_null() {
			let entitled = has_entitlement();
			let root = unsafe { libc::geteuid() } == 0;
			tracing::warn!(
				?entitled,
				root,
				"IOHIDUserDeviceCreateWithProperties refused the virtual pad"
			);
			return Err(match entitled {
				Some(true) => PadFailure::Other,
				_ => PadFailure::MacEntitlement,
			});
		}

		let neutral = match personality {
			Personality::Xbox => hid::xbox_input_report(&GamepadState::default()).to_vec(),
			Personality::Ds4 => hid::ds4_input_report(&GamepadState::default(), 0, 0).to_vec(),
		};
		let last_input = Arc::new(Mutex::new(neutral));
		let (tx, rx) = mpsc::channel();
		let rumble_tx = Arc::new(Mutex::new(Some(tx)));

		// SET_REPORT (output/feature written by the game): decode rumble, forward changes.
		let set_block = {
			let rumble_tx = rumble_tx.clone();
			let last_rumble = Mutex::new(None::<(u8, u8)>);
			RcBlock::new(move |_ty: u32, report_id: u32, report: *const u8, len: isize| -> i32 {
				if report.is_null() || len <= 0 {
					return IO_RETURN_SUCCESS;
				}
				let data = unsafe { std::slice::from_raw_parts(report, len as usize) };
				let motors = match personality {
					Personality::Xbox => hid::xbox_rumble(report_id, data),
					Personality::Ds4 => hid::ds4_rumble(report_id, data),
				};
				if let Some(m) = motors {
					let mut last = last_rumble.lock().unwrap_or_else(|e| e.into_inner());
					if *last != Some(m) {
						*last = Some(m);
						if let Some(tx) = rumble_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
							let _ = tx.send(m);
						}
					}
				}
				IO_RETURN_SUCCESS
			})
		};
		// GET_REPORT (read by a driver): the DS4 feature reports + the current input report.
		let get_block = {
			let last_input = last_input.clone();
			RcBlock::new(move |ty: u32, report_id: u32, report: *mut u8, len: *mut isize| -> i32 {
				if report.is_null() || len.is_null() {
					return IO_RETURN_UNSUPPORTED;
				}
				let src: Vec<u8> = match (personality, ty, report_id) {
					(Personality::Ds4, REPORT_TYPE_FEATURE, 0x02) => hid::ds4_calibration_report().to_vec(),
					(Personality::Ds4, REPORT_TYPE_FEATURE, 0x12) => hid::ds4_pairing_report(unique).to_vec(),
					(Personality::Ds4, REPORT_TYPE_FEATURE, 0xA3) => hid::ds4_firmware_report().to_vec(),
					(_, REPORT_TYPE_INPUT, 0x01) => last_input.lock().unwrap_or_else(|e| e.into_inner()).clone(),
					_ => return IO_RETURN_UNSUPPORTED,
				};
				let cap = unsafe { *len };
				if cap <= 0 {
					return IO_RETURN_UNSUPPORTED;
				}
				let n = src.len().min(cap as usize);
				unsafe {
					std::ptr::copy_nonoverlapping(src.as_ptr(), report, n);
					*len = n as isize;
				}
				IO_RETURN_SUCCESS
			})
		};

		let queue = unsafe { dispatch_queue_create(c"com.pulsar.virtual-pad".as_ptr(), std::ptr::null_mut()) };
		if queue.is_null() {
			// Not activated yet: a plain release is fine.
			unsafe { CFRelease(dev as CFTypeRef) };
			return Err(PadFailure::Other);
		}
		// Cancel handler: runs on the queue once the device is cancelled and every pending
		// callback is done — only then may the device and its queue be released.
		let held = Mutex::new(Some((set_block.clone(), get_block.clone())));
		let (dev_addr, queue_addr) = (dev as usize, queue as usize);
		let cancel_block = RcBlock::new(move || {
			drop(held.lock().unwrap_or_else(|e| e.into_inner()).take());
			unsafe {
				CFRelease(dev_addr as CFTypeRef);
				dispatch_release(queue_addr as *mut c_void);
			}
		});
		unsafe {
			(api.register_set)(dev, RcBlock::as_ptr(&set_block) as *mut c_void);
			(api.register_get)(dev, RcBlock::as_ptr(&get_block) as *mut c_void);
			(api.set_queue)(dev, queue);
			(api.set_cancel)(dev, RcBlock::as_ptr(&cancel_block) as *mut c_void);
			(api.activate)(dev);
		}
		tracing::info!(?personality, unique, "macOS virtual HID pad created");

		let mut pad = Self {
			kind,
			personality,
			api,
			dev,
			last_input,
			rumble_tx,
			rumble_rx: Mutex::new(Some(rx)),
			counter: 0,
			started: Instant::now(),
			warned: false,
		};
		pad.apply(&GamepadState::default());
		Ok(pad)
	}
}

/// The `IOHIDUserDevice` property dictionary (IOHIDKeys.h key names).
fn properties(personality: Personality, unique: u32) -> CFMutableDictionary<CFString, CFType> {
	let (desc, vid, pid, version, product, maker, transport) = match personality {
		Personality::Xbox => (
			hid::XBOX_DESCRIPTOR,
			hid::VID_MICROSOFT,
			hid::PID_XBOX_BT,
			0x0509,
			"Xbox Wireless Controller",
			"Microsoft",
			"Bluetooth",
		),
		Personality::Ds4 => (
			hid::DS4_DESCRIPTOR,
			hid::VID_SONY,
			hid::PID_DS4_V2,
			0x0100,
			"Wireless Controller",
			"Sony Interactive Entertainment",
			"USB",
		),
	};
	let mut d = CFMutableDictionary::<CFString, CFType>::new();
	let mut num = |k: &str, v: i32| d.set(CFString::new(k), CFNumber::from(v).as_CFType());
	num("VendorID", vid as i32);
	num("ProductID", pid as i32);
	num("VersionNumber", version);
	num("CountryCode", 0);
	num("PrimaryUsagePage", 0x01);
	num("PrimaryUsage", 0x05);
	num("LocationID", unique as i32);
	d.set(CFString::new("ReportDescriptor"), CFData::from_buffer(desc).as_CFType());
	d.set(CFString::new("Product"), CFString::new(product).as_CFType());
	d.set(CFString::new("Manufacturer"), CFString::new(maker).as_CFType());
	d.set(CFString::new("Transport"), CFString::new(transport).as_CFType());
	d.set(CFString::new("SerialNumber"), CFString::new(&format!("PULSAR-{unique:08X}")).as_CFType());
	d
}

impl VirtualGamepad for MacHidGamepad {
	fn kind(&self) -> GamepadKind {
		self.kind
	}

	fn apply(&mut self, state: &GamepadState) {
		let report = match self.personality {
			Personality::Xbox => hid::xbox_input_report(state).to_vec(),
			Personality::Ds4 => {
				self.counter = self.counter.wrapping_add(1) & 0x3F;
				// DS4 sensor clock: 5.33 µs units (3 per 16 µs), 16-bit wrap.
				let ts = (self.started.elapsed().as_micros() * 3 / 16) as u16;
				hid::ds4_input_report(state, self.counter, ts).to_vec()
			}
		};
		let rc = unsafe {
			(self.api.handle_report)(self.dev, mach_absolute_time(), report.as_ptr(), report.len() as isize)
		};
		*self.last_input.lock().unwrap_or_else(|e| e.into_inner()) = report;
		if rc != IO_RETURN_SUCCESS && !self.warned {
			self.warned = true;
			tracing::warn!(rc = format!("{rc:#010x}"), "macOS virtual pad: HandleReport failed");
		}
	}

	fn rumble_reader(&self) -> Option<Box<dyn RumbleReader>> {
		let rx = self.rumble_rx.lock().unwrap_or_else(|e| e.into_inner()).take()?;
		Some(Box::new(MacRumble { rx }))
	}
}

impl Drop for MacHidGamepad {
	fn drop(&mut self) {
		// Ends the rumble reader thread (its channel closes).
		*self.rumble_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
		// Release everything held before the device goes, so nothing stays pressed in the game.
		self.apply(&GamepadState::default());
		// The cancel handler releases the device + its queue once callbacks have drained.
		unsafe { (self.api.cancel)(self.dev) };
	}
}

struct MacRumble {
	rx: mpsc::Receiver<(u8, u8)>,
}

impl RumbleReader for MacRumble {
	fn next(&mut self) -> Option<(u8, u8)> {
		// Ends (None) when the pad is dropped: its sender is cleared in Drop.
		self.rx.recv().ok()
	}
}
