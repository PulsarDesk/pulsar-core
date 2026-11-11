//! Linux: plug the USB/IP-exported device into the in-kernel `vhci-hcd` directly through
//! sysfs, without the `usbip` CLI.
//!
//! This is what `usbip attach` does in `libsrc/vhci_driver.c` (`usbip_vhci_attach_device2`):
//!
//! 1. Connect a TCP socket to the server and run the `OP_REQ_IMPORT` handshake on it.
//! 2. Pick a free port from `/sys/devices/platform/vhci_hcd.0/status` (`hs` hub for a
//!    full-speed device, state 4 = `VDEV_ST_NULL`).
//! 3. Write `"<port> <sockfd> <devid> <speed>"` to `.../vhci_hcd.0/attach`, where `devid` is
//!    `busnum << 16 | devnum` and `speed` the `enum usb_device_speed` from the import reply.
//!    The kernel looks the fd up in the WRITING process (`sockfd_lookup`) and keeps its own
//!    reference, so our copy of the fd is closed right after the write.
//! 4. Unplug: write `"<port>"` to `.../vhci_hcd.0/detach`. The kernel shuts the socket down,
//!    which ends the server session.
//!
//! Privilege: the kernel checks nothing beyond the file mode (`attach`/`detach` are
//! `DEVICE_ATTR_WO`, root-only `0200`). Pulsar's udev rule (`70-pulsar-vhci.rules`, shipped
//! in the .deb) hands both files to the `pulsar-vhci` group with `g+w`, so a member can
//! attach without root. Being able to attach means being able to plug ANY virtual USB device
//! into this machine (the same as physical access to a USB port: a malicious device can
//! attack kernel drivers), which is why it is an explicit, opt-in group and not `uaccess`.
//! Design notes: `docs/dualsense.md` ("Linux").
//!
//! Unlike `usbip attach` we do not write `/var/run/vhci_hcd/port<N>` (the CLI's own record
//! of the remote host), so `usbip port` lists our device without its `usbip://` URL.

use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;

/// The first vhci controller's sysfs directory.
pub const VHCI_DIR: &str = "/sys/devices/platform/vhci_hcd.0";

/// `VDEV_ST_NULL`: a free port.
const ST_NULL: u32 = 4;

/// One row of the vhci `status` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
	/// High-speed hub (`hs`, takes low/full/high speed) or SuperSpeed (`ss`).
	pub high_speed_hub: bool,
	/// The port number `attach`/`detach` take.
	pub port: u32,
	/// `VDEV_ST_*`: 4 free, 5 not assigned, 6 used, 7 error.
	pub state: u32,
	/// The attached device's `busnum << 16 | devnum` (0 when free).
	pub devid: u32,
}

/// Parse `/sys/devices/platform/vhci_hcd.0/status`.
///
/// Since Linux 4.13: `hub port sta spd dev sockfd local_busid`, one row per port, e.g.
/// `hs  0000 004 000 00000000 000000 0-0`. Older kernels print `prt sta spd bus dev socket
/// local_busid` without a hub column; there every port is high speed.
pub fn parse_status(s: &str) -> Vec<PortStatus> {
	let mut lines = s.lines();
	let with_hub = lines
		.next()
		.is_some_and(|h| h.split_whitespace().next() == Some("hub"));
	lines
		.filter_map(|l| {
			let mut f = l.split_whitespace();
			let high_speed_hub = if with_hub {
				match f.next()? {
					"hs" => true,
					"ss" => false,
					_ => return None,
				}
			} else {
				true
			};
			let port = f.next()?.parse().ok()?;
			let state = f.next()?.parse().ok()?;
			let _speed = f.next()?;
			// Old format: separate bus and dev columns.
			let devid = if with_hub {
				u32::from_str_radix(f.next()?, 16).ok()?
			} else {
				let bus: u32 = f.next()?.parse().ok()?;
				let dev: u32 = f.next()?.parse().ok()?;
				bus << 16 | dev
			};
			Some(PortStatus {
				high_speed_hub,
				port,
				state,
				devid,
			})
		})
		.collect()
}

/// The free high-speed-hub ports (where a full-speed device goes), in order.
pub fn free_ports(status: &[PortStatus]) -> Vec<u32> {
	status
		.iter()
		.filter(|p| p.high_speed_hub && p.state == ST_NULL)
		.map(|p| p.port)
		.collect()
}

/// The `attach` line.
pub fn attach_line(port: u32, sockfd: i32, devid: u32, speed: u32) -> String {
	format!("{port} {sockfd} {devid} {speed}")
}

/// `(devid, speed)` from an `OP_REP_IMPORT` device record (312 bytes).
pub fn devid_speed(record: &[u8]) -> Option<(u32, u32)> {
	let be = |o: usize| -> Option<u32> {
		Some(u32::from_be_bytes(record.get(o..o + 4)?.try_into().ok()?))
	};
	let (bus, dev, speed) = (be(288)?, be(292)?, be(296)?);
	Some((bus << 16 | dev, speed))
}

/// Why sysfs attach is not possible here, or `Ok` if it is.
#[cfg(target_os = "linux")]
pub fn check() -> io::Result<()> {
	let dir = Path::new(VHCI_DIR);
	if !dir.join("attach").exists() {
		return Err(io::Error::new(
			io::ErrorKind::NotFound,
			"vhci-hcd is not loaded (sudo modprobe vhci-hcd; Pulsar's package loads it at boot \
			 via /etc/modules-load.d/pulsar.conf)",
		));
	}
	for f in ["attach", "detach"] {
		// Opening a sysfs store for writing without writing does nothing.
		if let Err(e) = std::fs::OpenOptions::new().write(true).open(dir.join(f)) {
			return Err(io::Error::new(
				e.kind(),
				format!(
					"cannot write {VHCI_DIR}/{f}: {e} (add the user to the pulsar-vhci group: \
					 sudo usermod -aG pulsar-vhci $USER, then log in again)"
				),
			));
		}
	}
	Ok(())
}

#[cfg(target_os = "linux")]
fn read_status() -> io::Result<Vec<PortStatus>> {
	Ok(parse_status(&std::fs::read_to_string(
		Path::new(VHCI_DIR).join("status"),
	)?))
}

#[cfg(target_os = "linux")]
fn write_attr(name: &str, line: &str) -> io::Result<()> {
	use std::io::Write;
	let mut f = std::fs::OpenOptions::new()
		.write(true)
		.open(Path::new(VHCI_DIR).join(name))?;
	// One write(2): a sysfs store sees exactly what one call hands it.
	f.write_all(line.as_bytes())
}

/// Connect to our server on 127.0.0.1:`tcp_port`, import `bus_id` and hand the socket to
/// vhci-hcd. Returns the vhci port.
#[cfg(target_os = "linux")]
pub fn attach(tcp_port: u16, bus_id: &str) -> io::Result<u32> {
	use super::usbip::{op_header, DEVICE_RECORD_LEN};
	use std::io::{Read, Write};
	use std::net::TcpStream;
	use std::os::fd::AsRawFd;
	use std::time::Duration;

	check()?;
	let port = *free_ports(&read_status()?)
		.first()
		.ok_or_else(|| io::Error::other("vhci-hcd: no free port"))?;

	let mut s = TcpStream::connect(("127.0.0.1", tcp_port))?;
	// Same socket options as `usbip_net_tcp_connect`.
	s.set_nodelay(true)?;
	socket2::SockRef::from(&s).set_keepalive(true)?;
	s.set_read_timeout(Some(Duration::from_secs(5)))?;
	let mut req = op_header(0x8003, 0).to_vec();
	let mut bus = [0u8; 32];
	bus[..bus_id.len()].copy_from_slice(bus_id.as_bytes());
	req.extend_from_slice(&bus);
	s.write_all(&req)?;
	let mut hdr = [0u8; 8];
	s.read_exact(&mut hdr)?;
	let status = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
	if status != 0 {
		return Err(io::Error::other(format!(
			"usbip import refused (status {status})"
		)));
	}
	let mut rec = [0u8; DEVICE_RECORD_LEN];
	s.read_exact(&mut rec)?;
	let (devid, speed) = devid_speed(&rec).unwrap();
	// The kernel's rx thread reads blocking; don't leave our timeout on the shared socket.
	s.set_read_timeout(None)?;
	let line = attach_line(port, s.as_raw_fd(), devid, speed);
	super::usbip::trace!("vhci: attach <- {line:?}");
	write_attr("attach", &line)
		.map_err(|e| io::Error::new(e.kind(), format!("vhci-hcd attach {line:?}: {e}")))?;
	// `s` drops here: closes OUR fd only; the kernel holds its own reference.
	Ok(port)
}

/// Unplug vhci `port`.
#[cfg(target_os = "linux")]
pub fn detach(port: u32) -> io::Result<()> {
	super::usbip::trace!("vhci: detach <- {port}");
	write_attr("detach", &port.to_string())
}

/// Not Linux: always unavailable.
#[cfg(not(target_os = "linux"))]
pub fn check() -> io::Result<()> {
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		"vhci-hcd sysfs attach is Linux-only",
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	const STATUS_5_10: &str = "hub port sta spd dev      sockfd local_busid\n\
		hs  0000 006 002 00010002 000003 3-1\n\
		hs  0001 004 000 00000000 000000 0-0\n\
		hs  0002 004 000 00000000 000000 0-0\n\
		ss  0008 004 000 00000000 000000 0-0\n";

	#[test]
	fn parses_status() {
		let st = parse_status(STATUS_5_10);
		assert_eq!(st.len(), 4);
		assert_eq!(
			st[0],
			PortStatus {
				high_speed_hub: true,
				port: 0,
				state: 6,
				devid: 0x0001_0002,
			}
		);
		assert!(!st[3].high_speed_hub);
		assert_eq!(st[3].port, 8);
		assert_eq!(free_ports(&st), vec![1, 2]);
	}

	#[test]
	fn parses_old_status() {
		let st = parse_status(
			"prt sta spd bus dev socket           local_busid\n\
			 000 004 000 000 000 0000000000000000 0-0\n\
			 001 006 002 001 002 ffff88003a0e9c00 1-1\n",
		);
		assert_eq!(free_ports(&st), vec![0]);
		assert_eq!(st[1].devid, 0x0001_0002);
	}

	#[test]
	fn builds_attach_line_from_import_record() {
		let mut rec = [0u8; 312];
		rec[288..292].copy_from_slice(&1u32.to_be_bytes());
		rec[292..296].copy_from_slice(&1u32.to_be_bytes());
		rec[296..300].copy_from_slice(&2u32.to_be_bytes());
		let (devid, speed) = devid_speed(&rec).unwrap();
		assert_eq!(attach_line(3, 7, devid, speed), "3 7 65537 2");
		assert_eq!(devid_speed(&rec[..100]), None);
	}
}
