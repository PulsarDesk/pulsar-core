//! "The machine's network just changed" events: an adapter came up / went down or an IPv4
//! address was added / removed. LAN discovery listens so it can rejoin its multicast group
//! at once — after sleep / resume, a Wi-Fi reconnect or a VPN (ZeroTier, Tailscale) adapter
//! restart the OS silently drops the socket's group membership, and without a rejoin the
//! node stops hearing every other device until the app restarts (seen 2026-11-13: a laptop
//! woke from sleep and no longer listed the home PC on ZeroTier, while the beacons were
//! still arriving at the adapter).
//!
//! One OS watcher per process, started on first use:
//! * Windows: `NotifyIpInterfaceChange` + `NotifyUnicastIpAddressChange` (IPv4);
//! * Linux: a `NETLINK_ROUTE` socket on the link + IPv4 address groups;
//! * macOS: a `PF_ROUTE` socket, address / interface messages only.
//!
//! Elsewhere (Android, iOS) there is no watcher and [`subscribe`] never fires; callers keep a
//! slow fallback timer.

use std::sync::OnceLock;

use tokio::sync::watch;

static TX: OnceLock<watch::Sender<u64>> = OnceLock::new();

/// Bumped on every network change. The first call starts the OS watcher.
pub fn subscribe() -> watch::Receiver<u64> {
	TX.get_or_init(|| {
		let (tx, _) = watch::channel(0u64);
		start();
		tx
	})
	.subscribe()
}

fn bump() {
	if let Some(tx) = TX.get() {
		tx.send_modify(|n| *n = n.wrapping_add(1));
	}
}

#[cfg(windows)]
fn start() {
	use std::ffi::c_void;
	use windows_sys::Win32::Foundation::HANDLE;
	use windows_sys::Win32::NetworkManagement::IpHelper::{
		NotifyIpInterfaceChange, NotifyUnicastIpAddressChange, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
		MIB_UNICASTIPADDRESS_ROW,
	};
	use windows_sys::Win32::Networking::WinSock::AF_INET;

	// Called on a system thread-pool thread; TX is set before the first event (start() runs
	// inside get_or_init, bump() before that point just finds no sender and is a no-op).
	unsafe extern "system" fn on_iface(_: *const c_void, _: *const MIB_IPINTERFACE_ROW, _: MIB_NOTIFICATION_TYPE) {
		bump();
	}
	unsafe extern "system" fn on_addr(_: *const c_void, _: *const MIB_UNICASTIPADDRESS_ROW, _: MIB_NOTIFICATION_TYPE) {
		bump();
	}
	// The registrations live for the whole process (the handles are never cancelled).
	let mut h1: HANDLE = std::ptr::null_mut();
	let mut h2: HANDLE = std::ptr::null_mut();
	// SAFETY: plain registrations with static callbacks and no caller context.
	let (r1, r2) = unsafe {
		(
			NotifyIpInterfaceChange(AF_INET, Some(on_iface), std::ptr::null(), false, &mut h1),
			NotifyUnicastIpAddressChange(AF_INET, Some(on_addr), std::ptr::null(), false, &mut h2),
		)
	};
	tracing::debug!(r1, r2, "netwatch: Windows IP change notifications");
}

#[cfg(target_os = "linux")]
fn start() {
	let spawned = std::thread::Builder::new().name("pulsar-netwatch".into()).spawn(|| {
		// SAFETY: a netlink socket + bind with a zeroed, filled sockaddr_nl; recv into a local
		// buffer of its stated length.
		unsafe {
			let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE);
			if fd < 0 {
				tracing::debug!("netwatch: netlink socket failed");
				return;
			}
			let mut sa: libc::sockaddr_nl = std::mem::zeroed();
			sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
			sa.nl_groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR) as u32;
			if libc::bind(
				fd,
				&sa as *const libc::sockaddr_nl as *const libc::sockaddr,
				std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
			) < 0
			{
				tracing::debug!("netwatch: netlink bind failed");
				libc::close(fd);
				return;
			}
			let mut buf = [0u8; 8192];
			loop {
				let n = libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0);
				if n > 0 {
					bump();
					continue;
				}
				match std::io::Error::last_os_error().raw_os_error() {
					// Interrupted, or the kernel dropped messages (a burst): still a change.
					Some(libc::EINTR) => {}
					Some(libc::ENOBUFS) => bump(),
					_ => std::thread::sleep(std::time::Duration::from_secs(1)),
				}
			}
		}
	});
	if spawned.is_err() {
		tracing::debug!("netwatch: thread spawn failed");
	}
}

#[cfg(target_os = "macos")]
fn start() {
	let spawned = std::thread::Builder::new().name("pulsar-netwatch".into()).spawn(|| {
		// SAFETY: a routing socket; read into a local buffer of its stated length; the header
		// byte read is bounds-checked against the returned length.
		unsafe {
			let fd = libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC);
			if fd < 0 {
				tracing::debug!("netwatch: route socket failed");
				return;
			}
			let mut buf = [0u8; 4096];
			loop {
				let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
				if n < 0 {
					if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
						std::thread::sleep(std::time::Duration::from_secs(1));
					}
					continue;
				}
				// rt_msghdr: u16 msglen, u8 version, u8 type. Route / ARP churn (RTM_ADD,
				// RTM_DELETE, RTM_MISS…) is frequent and irrelevant — only address and
				// interface-state messages count.
				// <net/route.h> values (not all exported by the libc crate).
				const RTM_NEWADDR: u8 = 0xc;
				const RTM_DELADDR: u8 = 0xd;
				const RTM_IFINFO: u8 = 0xe;
				if n >= 4 {
					let t = buf[3];
					if t == RTM_NEWADDR || t == RTM_DELADDR || t == RTM_IFINFO {
						bump();
					}
				}
			}
		}
	});
	if spawned.is_err() {
		tracing::debug!("netwatch: thread spawn failed");
	}
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn start() {}
