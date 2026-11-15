//! Network-path facts shared by every app: what kind of path a session runs on (a real LAN,
//! the internet, an overlay VPN, the relay), and whether the media socket's kernel buffers
//! are big enough for a stream's bursts.
//!
//! Lives in the core so the desktop and mobile clients classify a path the same way (they
//! used to copy the logic, and the copies drifted).

use std::net::IpAddr;

use crate::connection::Transport;

// ── Path classification ──────────────────────────────────────────────────────────────────

/// What kind of network path a session runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
	/// Direct to a peer on the same local network (private address, not an overlay VPN,
	/// low round trip).
	Lan,
	/// Direct over the internet: a public address, a punched mobile / CGNAT path, or an
	/// overlay VPN (ZeroTier, WireGuard, Tailscale, ...) whose private addresses cross the
	/// internet underneath.
	Wan,
	/// Relayed by the rendezvous server.
	Relay,
}

impl PathClass {
	pub fn as_str(self) -> &'static str {
		match self {
			PathClass::Lan => "lan",
			PathClass::Wan => "wan",
			PathClass::Relay => "relay",
		}
	}
}

/// A direct path to a private address with a round trip at or above this is not treated as a
/// LAN: a wired or Wi-Fi LAN answers in a few ms (a two-hop mesh in ~4-7 ms), while an
/// overlay VPN the interface check could not recognise (a WireGuard tunnel named by the user)
/// still pays the internet's round trip.
pub const LAN_MAX_RTT_MS: f32 = 12.0;

/// Classify a live path: relayed, or direct to `peer` (its address; unknown → `Wan`, the
/// cautious answer — a wrong `Lan` seeds a thin path with a LAN rate and picks uncompressed
/// audio, which has no loss concealment). `rtt_ms` is the measured path round trip, when
/// known.
pub fn classify(transport: Transport, peer: Option<IpAddr>, rtt_ms: Option<f32>) -> PathClass {
	match transport {
		Transport::Relay => PathClass::Relay,
		Transport::Direct => match peer {
			Some(ip) if is_lan_peer(ip, rtt_ms) => PathClass::Lan,
			_ => PathClass::Wan,
		},
	}
}

/// The pure part of [`classify`] for a direct path: a local address that is not on an
/// overlay VPN, with no round trip that betrays the internet underneath.
fn is_lan_peer(ip: IpAddr, rtt_ms: Option<f32>) -> bool {
	is_local_ip(ip) && rtt_ms.is_none_or(|r| r < LAN_MAX_RTT_MS) && !is_overlay_addr(ip)
}

/// Private (RFC 1918 / IPv6 unique-local), link-local or loopback. The CGNAT shared space
/// (100.64/10) is NOT local: a direct path to it is a carrier network or an overlay VPN
/// (Tailscale) that crosses the internet.
pub fn is_local_ip(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
		IpAddr::V6(v6) => {
			if let Some(v4) = v6.to_ipv4_mapped() {
				return is_local_ip(IpAddr::V4(v4));
			}
			let s0 = v6.segments()[0];
			v6.is_loopback()
				// fc00::/7 unique-local
				|| (s0 & 0xfe00) == 0xfc00
				// fe80::/10 link-local
				|| (s0 & 0xffc0) == 0xfe80
		}
	}
}

/// True when `ip` is reached over a local VPN / overlay adapter (ZeroTier, Tailscale,
/// WireGuard, Hamachi, ...) or sits in Tailscale's CGNAT range. Such a peer LOOKS same-LAN (a
/// private address, even a LAN-discovery beacon: ZeroTier carries multicast), but every packet
/// crosses the internet inside the overlay's encapsulation, with a ~1280-2800 byte MTU.
///
/// Checked three ways: the interface whose subnet holds `ip` (name), the interface the OS
/// would route `ip` over (Windows: adapter type + description; Linux: name + kernel link
/// type), and Tailscale's 100.64/10.
pub fn is_overlay_addr(ip: IpAddr) -> bool {
	let IpAddr::V4(peer) = ip else {
		return false;
	};
	let o = peer.octets();
	if o[0] == 100 && (64..128).contains(&o[1]) {
		return true; // 100.64.0.0/10 — Tailscale / carrier-grade NAT, never a real LAN
	}
	if peer.is_loopback() {
		return false;
	}
	let on_overlay_subnet = if_addrs::get_if_addrs()
		.map(|ifs| {
			ifs.iter().any(|i| match &i.addr {
				if_addrs::IfAddr::V4(a) => {
					let mask = u32::from(a.netmask);
					mask != 0
						&& (u32::from(a.ip) & mask) == (u32::from(peer) & mask)
						&& is_overlay_iface_name(&i.name)
				}
				_ => false,
			})
		})
		.unwrap_or(false);
	on_overlay_subnet || route::overlay_route(peer)
}

/// Interface names of VPN / overlay adapters: Windows friendly names ("ZeroTier One [..]",
/// "Tailscale", "WireGuard Tunnel", "Hamachi") and Unix names (`zt*`, `tailscale0`, `wg*`,
/// `tun*`, `utun*` (macOS WireGuard / Tailscale), `feth*` (macOS ZeroTier), `ham*`).
pub fn is_overlay_iface_name(name: &str) -> bool {
	let n = name.to_ascii_lowercase();
	const WORDS: &[&str] = &[
		"zerotier", "tailscale", "wireguard", "hamachi", "radmin", "openvpn", "tap-windows", "wintun",
		"nordlynx", "mullvad", "protonvpn", "netbird", "nebula",
	];
	const PREFIXES: &[&str] = &["zt", "wg", "tun", "utun", "feth", "ham"];
	WORDS.iter().any(|w| n.contains(w)) || PREFIXES.iter().any(|p| n.starts_with(p))
}

/// Virtual / container / hypervisor bridges: their private addresses are never a path to
/// another machine on the user's LAN.
pub fn is_virtual_iface_name(name: &str) -> bool {
	let n = name.to_ascii_lowercase();
	const WORDS: &[&str] = &["vethernet", "hyper-v", "virtualbox", "vmware", "vmnet", "docker", "wsl"];
	const PREFIXES: &[&str] = &["docker", "br-", "veth", "virbr", "vboxnet", "lxc", "lxd", "cni", "flannel", "podman", "vmnet"];
	WORDS.iter().any(|w| n.contains(w)) || PREFIXES.iter().any(|p| n.starts_with(p))
}

#[cfg(windows)]
mod route {
	use std::net::Ipv4Addr;
	use windows_sys::Win32::NetworkManagement::IpHelper::{
		GetAdaptersAddresses, GetBestInterfaceEx, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
		GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH,
	};
	use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR, SOCKADDR_IN};

	/// IF_TYPE_PROP_VIRTUAL (WireGuard/Wintun, ZeroTier, Tailscale), IF_TYPE_TUNNEL, IF_TYPE_PPP
	/// (classic dial-up style VPNs).
	const OVERLAY_IF_TYPES: [u32; 3] = [53, 131, 23];

	fn wide(p: *const u16) -> String {
		if p.is_null() {
			return String::new();
		}
		let mut n = 0;
		// SAFETY: a NUL-terminated wide string owned by the GetAdaptersAddresses buffer.
		unsafe {
			while *p.add(n) != 0 {
				n += 1;
			}
			String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
		}
	}

	fn best_if(ip: Ipv4Addr) -> Option<u32> {
		// SAFETY: plain-old-data struct, all-zero is a valid value.
		let mut sin: SOCKADDR_IN = unsafe { std::mem::zeroed() };
		sin.sin_family = AF_INET;
		sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(ip.octets());
		let mut idx = 0u32;
		// SAFETY: a valid IPv4 sockaddr; GetBestInterfaceEx only reads it (no packet is sent).
		let rc = unsafe { GetBestInterfaceEx(&sin as *const SOCKADDR_IN as *const SOCKADDR, &mut idx) };
		(rc == 0).then_some(idx)
	}

	/// (type, description, friendly name) of the adapter with interface index `index`.
	fn adapter(index: u32) -> Option<(u32, String, String)> {
		let flags = GAA_FLAG_SKIP_UNICAST | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
		let mut size: u32 = 32 * 1024;
		for _ in 0..3 {
			// u64 storage keeps the buffer aligned for IP_ADAPTER_ADDRESSES_LH.
			let mut buf = vec![0u64; (size as usize).div_ceil(8)];
			let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
			// SAFETY: `buf` holds `size` bytes; the API fills it with a linked list inside it.
			let rc = unsafe { GetAdaptersAddresses(AF_UNSPEC as u32, flags, std::ptr::null(), head, &mut size) };
			if rc == 111 {
				continue; // ERROR_BUFFER_OVERFLOW: `size` now holds what it needs
			}
			if rc != 0 {
				return None;
			}
			let mut cur = head as *const IP_ADAPTER_ADDRESSES_LH;
			while !cur.is_null() {
				// SAFETY: a node of the list the API built inside `buf` (alive in this scope).
				let a = unsafe { &*cur };
				// SAFETY: the union's struct view (Length, IfIndex) is what the API fills.
				if unsafe { a.Anonymous1.Anonymous.IfIndex } == index {
					return Some((a.IfType, wide(a.Description), wide(a.FriendlyName)));
				}
				cur = a.Next;
			}
			return None;
		}
		None
	}

	/// The adapter Windows routes `peer` over is a VPN / overlay one.
	pub(super) fn overlay_route(peer: Ipv4Addr) -> bool {
		let Some((if_type, desc, name)) = best_if(peer).and_then(adapter) else {
			return false;
		};
		OVERLAY_IF_TYPES.contains(&if_type) || super::is_overlay_iface_name(&desc) || super::is_overlay_iface_name(&name)
	}
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod route {
	use std::net::Ipv4Addr;

	/// ARPHRD_NONE: point-to-point links without a link layer — WireGuard, tun (OpenVPN,
	/// Tailscale's tailscale0, Android VpnService), PPP-style tunnels.
	const ARPHRD_NONE: u32 = 65534;
	/// ARPHRD_PPP.
	const ARPHRD_PPP: u32 = 512;

	/// The interface of the most specific `/proc/net/route` entry for `peer`.
	fn route_dev(table: &str, peer: Ipv4Addr) -> Option<String> {
		let p = u32::from(peer);
		let mut best: Option<(u32, u32, String)> = None; // (prefix len, metric, dev)
		for l in table.lines().skip(1) {
			let f: Vec<&str> = l.split_whitespace().collect();
			if f.len() < 8 {
				continue;
			}
			// Little-endian hex of the network-order address (as the kernel prints it).
			let (Ok(dst), Ok(mask)) = (u32::from_str_radix(f[1], 16), u32::from_str_radix(f[7], 16)) else {
				continue;
			};
			let (dst, mask) = (u32::from_be_bytes(dst.to_le_bytes()), u32::from_be_bytes(mask.to_le_bytes()));
			if p & mask != dst & mask {
				continue;
			}
			let len = mask.count_ones();
			let metric = f[6].parse().unwrap_or(u32::MAX);
			if best.as_ref().is_none_or(|b| len > b.0 || (len == b.0 && metric < b.1)) {
				best = Some((len, metric, f[0].to_string()));
			}
		}
		best.map(|b| b.2)
	}

	pub(super) fn overlay_route(peer: Ipv4Addr) -> bool {
		// Android 10+ hides /proc/net/route from apps: then only the subnet/name check runs.
		let Some(dev) = std::fs::read_to_string("/proc/net/route")
			.ok()
			.and_then(|t| route_dev(&t, peer))
		else {
			return false;
		};
		if super::is_overlay_iface_name(&dev) {
			return true;
		}
		let sys = format!("/sys/class/net/{dev}");
		let link_type = std::fs::read_to_string(format!("{sys}/type"))
			.ok()
			.and_then(|t| t.trim().parse::<u32>().ok());
		matches!(link_type, Some(ARPHRD_NONE | ARPHRD_PPP)) || std::path::Path::new(&format!("{sys}/tun_flags")).exists()
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn most_specific_route_wins() {
			// Default via eth0, 10.121.0.0/16 via ztabc (ZeroTier), 10.8.0.0/24 via wg0.
			let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
				eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n\
				ztabc\t0000790A\t00000000\t0001\t0\t0\t0\t0000FFFF\n\
				wg0\t0000080A\t00000000\t0001\t0\t0\t0\t00FFFFFF\n";
			assert_eq!(route_dev(table, Ipv4Addr::new(10, 121, 15, 149)).as_deref(), Some("ztabc"));
			assert_eq!(route_dev(table, Ipv4Addr::new(10, 8, 0, 3)).as_deref(), Some("wg0"));
			assert_eq!(route_dev(table, Ipv4Addr::new(192, 168, 1, 20)).as_deref(), Some("eth0"));
		}
	}
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
mod route {
	/// macOS / iOS: no cheap route lookup here; the subnet + interface-name check (`utun*`,
	/// `feth*`, `zt*`) and the RTT fallback in [`super::classify`] cover the overlays.
	pub(super) fn overlay_route(_peer: std::net::Ipv4Addr) -> bool {
		false
	}
}

// ── UDP socket buffers ───────────────────────────────────────────────────────────────────

/// What the media socket asks the kernel for, each way.
pub const UDP_BUF_WANT: usize = 4 << 20;
/// Below this effective receive buffer a 30-150 Mbit stream's keyframe / FEC burst overflows
/// the socket (Linux counts it as `UdpRcvbufErrors`) and the drop looks like network loss.
pub const UDP_BUF_MIN_OK: usize = 1 << 20;

/// The kernel buffer sizes a UDP socket actually got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpBufReport {
	pub requested: usize,
	/// Effective receive buffer in bytes (Linux reports twice the usable size; halved here).
	pub recv: usize,
	/// Effective send buffer in bytes.
	pub send: usize,
	/// `SO_RCVBUFFORCE` (Linux, needs CAP_NET_ADMIN) raised the receive buffer past
	/// `net.core.rmem_max`.
	pub forced: bool,
}

impl UdpBufReport {
	/// Big enough for a stream's bursts.
	pub fn ok(&self) -> bool {
		self.recv >= UDP_BUF_MIN_OK
	}
}

/// The Linux kernel reports `SO_RCVBUF`/`SO_SNDBUF` doubled (bookkeeping overhead); the
/// other kernels report what they grant.
fn effective(reported: usize) -> usize {
	if cfg!(any(target_os = "linux", target_os = "android")) {
		reported / 2
	} else {
		reported
	}
}

/// Ask for `want` bytes each way on `sock` and report what the kernel granted. Linux clamps
/// the request silently to `net.core.rmem_max`/`wmem_max` (stock: 208 KiB); when that
/// happened `SO_RCVBUFFORCE`/`SO_SNDBUFFORCE` is tried, which works only with CAP_NET_ADMIN
/// (a root relay, a granted binary) — the packaged sysctl drop-in (`60-pulsar.conf`) is the
/// real fix for a desktop. Best effort: never fails.
pub fn tune_udp_buffers(sock: &socket2::Socket, want: usize) -> UdpBufReport {
	let _ = sock.set_recv_buffer_size(want);
	let _ = sock.set_send_buffer_size(want);
	let recv_of = |s: &socket2::Socket| s.recv_buffer_size().map(effective).unwrap_or(0);
	let send_of = |s: &socket2::Socket| s.send_buffer_size().map(effective).unwrap_or(0);
	#[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
	let mut forced = false;
	#[cfg(target_os = "linux")]
	{
		use std::os::fd::AsRawFd;
		let force = |opt: libc::c_int| {
			let v: libc::c_int = want.min(i32::MAX as usize) as libc::c_int;
			// SAFETY: a valid socket fd and a c_int option value of the right size.
			unsafe {
				libc::setsockopt(
					sock.as_raw_fd(),
					libc::SOL_SOCKET,
					opt,
					&v as *const libc::c_int as *const libc::c_void,
					std::mem::size_of::<libc::c_int>() as libc::socklen_t,
				) == 0
			}
		};
		if recv_of(sock) < want {
			forced |= force(libc::SO_RCVBUFFORCE);
		}
		if send_of(sock) < want {
			let _ = force(libc::SO_SNDBUFFORCE);
		}
	}
	UdpBufReport {
		requested: want,
		recv: recv_of(sock),
		send: send_of(sock),
		forced,
	}
}

/// The OS-specific hint for a too-small buffer (logs and `--selftest`).
pub fn udp_buf_hint() -> &'static str {
	if cfg!(target_os = "linux") {
		"raise net.core.rmem_max/wmem_max: sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216 (the Pulsar packages install /usr/lib/sysctl.d/60-pulsar.conf for this)"
	} else if cfg!(target_os = "android") {
		"the system caps app socket buffers (net.core.rmem_max); nothing an app can change"
	} else {
		"the OS refused the requested socket buffer size"
	}
}

/// Bind a throwaway UDP socket and report what [`tune_udp_buffers`] gets on this machine
/// (`--selftest`).
pub fn udp_buffer_selfcheck() -> std::io::Result<UdpBufReport> {
	use socket2::{Domain, Protocol, Socket, Type};
	let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
	Ok(tune_udp_buffers(&s, UDP_BUF_WANT))
}

/// Linux: the machine-wide `Udp: RcvbufErrors` counter (datagrams dropped because a socket's
/// receive buffer was full). `None` elsewhere or when unreadable.
pub fn udp_rcvbuf_errors() -> Option<u64> {
	#[cfg(any(target_os = "linux", target_os = "android"))]
	{
		parse_rcvbuf_errors(&std::fs::read_to_string("/proc/net/snmp").ok()?)
	}
	#[cfg(not(any(target_os = "linux", target_os = "android")))]
	{
		None
	}
}

/// `/proc/net/snmp` has a header line and a value line per protocol.
#[cfg_attr(not(any(target_os = "linux", target_os = "android", test)), allow(dead_code))]
fn parse_rcvbuf_errors(snmp: &str) -> Option<u64> {
	let mut udp = snmp.lines().filter(|l| l.starts_with("Udp:"));
	let head = udp.next()?;
	let vals = udp.next()?;
	let col = head.split_whitespace().position(|c| c == "RcvbufErrors")?;
	vals.split_whitespace().nth(col)?.parse().ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn relay_is_relay_and_unknown_is_wan() {
		assert_eq!(classify(Transport::Relay, Some("192.168.1.2".parse().unwrap()), Some(1.0)), PathClass::Relay);
		assert_eq!(classify(Transport::Direct, None, None), PathClass::Wan);
	}

	#[test]
	fn public_and_cgnat_are_wan() {
		assert_eq!(classify(Transport::Direct, Some("203.0.113.9".parse().unwrap()), Some(3.0)), PathClass::Wan);
		// Tailscale / CGNAT.
		assert_eq!(classify(Transport::Direct, Some("100.101.1.2".parse().unwrap()), Some(3.0)), PathClass::Wan);
	}

	#[test]
	fn private_with_internet_rtt_is_wan() {
		// An overlay the interface check missed: private address, city-scale round trip.
		assert!(!is_lan_peer("10.8.0.3".parse().unwrap(), Some(14.0)));
		assert!(!is_lan_peer("10.8.0.3".parse().unwrap(), Some(LAN_MAX_RTT_MS)));
	}

	#[test]
	fn loopback_is_lan() {
		assert_eq!(classify(Transport::Direct, Some("127.0.0.1".parse().unwrap()), Some(0.2)), PathClass::Lan);
		assert_eq!(classify(Transport::Direct, Some("127.0.0.1".parse().unwrap()), None), PathClass::Lan);
	}

	#[test]
	fn overlay_and_virtual_names() {
		for n in ["ZeroTier One [8ade57abc3]", "zt7nnig26", "wg0", "tailscale0", "utun4", "feth2301", "tun0", "Tailscale"] {
			assert!(is_overlay_iface_name(n), "{n}");
		}
		for n in ["eth0", "Wi-Fi", "Ethernet", "wlan0", "en0", "enp3s0"] {
			assert!(!is_overlay_iface_name(n), "{n}");
		}
		for n in ["vEthernet (Default Switch)", "docker0", "br-1a2b", "virbr0", "vboxnet0", "veth12ab"] {
			assert!(is_virtual_iface_name(n), "{n}");
		}
		assert!(!is_virtual_iface_name("Ethernet 2"));
	}

	#[test]
	fn rcvbuf_errors_parse() {
		let snmp = "Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors InCsumErrors IgnoredMulti MemErrors\n\
			Udp: 1000 2 7 900 5 0 0 3 0\n";
		assert_eq!(parse_rcvbuf_errors(snmp), Some(5));
		assert_eq!(parse_rcvbuf_errors("Ip: 1 2\nIp: 3 4\n"), None);
	}

	#[test]
	fn buffer_report_is_sane() {
		let r = udp_buffer_selfcheck().expect("bind");
		assert_eq!(r.requested, UDP_BUF_WANT);
		assert!(r.recv > 0 && r.send > 0, "{r:?}");
	}
}
