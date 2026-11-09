//! Router port mapping for the node's UDP socket: NAT-PMP (RFC 6886) first, then UPnP IGD.
//!
//! Why: some home routers drop a peer's hole-punch packets even after we sent ours — measured
//! 2026-11-08 between a home router (itself behind the ISP's carrier-grade NAT) and a laptop on
//! mobile data: both NATs map endpoint-independently, yet plain simultaneous punching got 0 of
//! ~240 packets through in 15 s, each way. With a NAT-PMP mapping for the socket on the router,
//! the first packet crossed in 35 ms, both ways, ~100 % delivery. That mapping is also what
//! ZeroTier creates, which is why ZeroTier found a direct path between the same two machines
//! while Pulsar fell back to the relay.
//!
//! A mapping makes the router forward ANY inbound packet for our port to us, so the peer's
//! punch is let in even before ours reached it. Behind a second (carrier) NAT it opens the
//! router's layer only — which is the layer that was blocking above.
//!
//! Best effort throughout: no answer within a few hundred ms (most networks: a phone hotspot,
//! a carrier, a corporate LAN) and the node carries on exactly as before.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Notify;
use tokio::time::timeout;

const NATPMP_PORT: u16 = 5351;
/// Requested lease. Renewed every [`REFRESH`], so a crashed app's mapping lapses within the hour.
const LEASE_SECS: u32 = 3600;
const REFRESH: Duration = Duration::from_secs(600);
const UPNP_DESCRIPTION: &str = "Pulsar";

/// A mapping the router granted.
#[derive(Clone, Debug)]
pub(super) enum Mapping {
	NatPmp {
		gateway: Ipv4Addr,
		internal: u16,
		external: u16,
	},
	Upnp {
		control: String,
		service: String,
		internal: u16,
		external: u16,
		/// The router only takes permanent (lease 0) mappings — removed on shutdown instead.
		permanent: bool,
	},
}

impl Mapping {
	fn external(&self) -> u16 {
		match self {
			Mapping::NatPmp { external, .. } | Mapping::Upnp { external, .. } => *external,
		}
	}
}

/// Which node currently owns the mapping of each external port. A re-bound node (settings
/// change → `go_online`) maps the same port again while the old node's keeper is still
/// winding down; only the CURRENT owner may delete the mapping, or the old node's late
/// delete would remove the new node's fresh mapping.
static OWNERS: std::sync::Mutex<Option<HashMap<u16, u64>>> = std::sync::Mutex::new(None);
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn claim(port: u16) -> u64 {
	let id = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
	OWNERS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.get_or_insert_with(HashMap::new)
		.insert(port, id);
	id
}

/// Release `port` if `id` still owns it; true = the caller should delete the mapping.
fn release(port: u16, id: u64) -> bool {
	let mut g = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
	let owners = g.get_or_insert_with(HashMap::new);
	if owners.get(&port) == Some(&id) {
		owners.remove(&port);
		true
	} else {
		false
	}
}

/// Map `internal` (the node's bound UDP port) on the local router and keep the mapping alive
/// until `stop` fires. NAT-PMP is tried inline (a few hundred ms at most), so a router that
/// speaks it has the mapping in place before the caller registers with the relay; UPnP needs a
/// multicast discovery round and runs in the background. `local_ip` is the address of the
/// interface the node's traffic leaves through.
pub(super) async fn start(internal: u16, local_ip: Ipv4Addr, stop: Arc<Notify>) {
	if let Some(m) = natpmp_map(internal, local_ip).await {
		tokio::spawn(keep(m, local_ip, stop));
		return;
	}
	tokio::spawn(async move {
		match upnp_map(internal, local_ip).await {
			Some(m) => keep(m, local_ip, stop).await,
			None => tracing::info!(
				port = internal,
				"port mapping: no NAT-PMP or UPnP router answered — hole punching relies on the NATs alone"
			),
		}
	});
}

/// Renew `m` every [`REFRESH`] until `stop`, then delete it (if this node still owns it).
async fn keep(m: Mapping, local_ip: Ipv4Addr, stop: Arc<Notify>) {
	let owner = claim(m.external());
	let mut m = m;
	loop {
		tokio::select! {
			_ = tokio::time::sleep(REFRESH) => {
				let renewed = match &m {
					Mapping::NatPmp { internal, .. } => natpmp_map_quiet(*internal, local_ip).await,
					Mapping::Upnp { internal, .. } => upnp_map_quiet(*internal, local_ip).await,
				};
				match renewed {
					Some(n) => m = n,
					None => tracing::warn!(?m, "port mapping: renewal failed — keeping the old lease until it lapses"),
				}
			}
			_ = stop.notified() => break,
		}
	}
	if release(m.external(), owner) {
		remove(&m).await;
	}
}

// ---------------------------------------------------------------- gateway discovery

/// Router candidates for NAT-PMP: the OS default gateway when the OS tells us (private only —
/// NAT-PMP is a LAN protocol), else `x.y.z.1` of our own private subnet (most home routers).
/// One candidate, so a network without NAT-PMP costs at most one short retry round before
/// the relay registration.
fn gateway_candidates(local_ip: Ipv4Addr) -> Vec<Ipv4Addr> {
	if let Some(g) = default_gateway() {
		return if g.is_private() { vec![g] } else { Vec::new() };
	}
	if local_ip.is_private() {
		let o = local_ip.octets();
		let g = Ipv4Addr::new(o[0], o[1], o[2], 1);
		if g != local_ip {
			return vec![g];
		}
	}
	Vec::new()
}

#[cfg(windows)]
fn default_gateway() -> Option<Ipv4Addr> {
	use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
	// The route the OS would use to reach the internet; its next hop is the default gateway.
	// Addresses are in network byte order in memory.
	let dest = u32::from_ne_bytes([8, 8, 8, 8]);
	let mut row: MIB_IPFORWARDROW = unsafe { std::mem::zeroed() };
	if unsafe { GetBestRoute(dest, 0, &mut row) } != 0 {
		return None;
	}
	let gw = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
	(!gw.is_unspecified()).then_some(gw)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn default_gateway() -> Option<Ipv4Addr> {
	// /proc/net/route: Iface Destination Gateway Flags ... — hex of the network-order u32 as a
	// host integer (so `to_ne_bytes` restores the address bytes on either endianness).
	let table = std::fs::read_to_string("/proc/net/route").ok()?;
	table.lines().skip(1).find_map(|line| {
		let f: Vec<&str> = line.split_whitespace().collect();
		let (dest, gw, flags) = (f.get(1)?, f.get(2)?, u32::from_str_radix(f.get(3)?, 16).ok()?);
		const RTF_UP: u32 = 0x1;
		const RTF_GATEWAY: u32 = 0x2;
		if *dest != "00000000" || flags & (RTF_UP | RTF_GATEWAY) != (RTF_UP | RTF_GATEWAY) {
			return None;
		}
		let gw = Ipv4Addr::from(u32::from_str_radix(gw, 16).ok()?.to_ne_bytes());
		(!gw.is_unspecified()).then_some(gw)
	})
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
fn default_gateway() -> Option<Ipv4Addr> {
	None
}

// ---------------------------------------------------------------- NAT-PMP (RFC 6886)

async fn natpmp_map(internal: u16, local_ip: Ipv4Addr) -> Option<Mapping> {
	let m = natpmp_map_quiet(internal, local_ip).await?;
	if let Mapping::NatPmp { gateway, external, .. } = &m {
		let wan = natpmp_external_ip(*gateway).await;
		tracing::info!(
			port = internal,
			external,
			%gateway,
			router_wan = ?wan,
			behind_carrier_nat = wan.is_some_and(is_cgnat),
			"port mapping: NAT-PMP mapped the node's UDP port on the router"
		);
	}
	Some(m)
}

async fn natpmp_map_quiet(internal: u16, local_ip: Ipv4Addr) -> Option<Mapping> {
	for gateway in gateway_candidates(local_ip) {
		// version 0, opcode 1 (UDP), reserved, internal port, suggested external port, lease.
		let mut req = vec![0u8, 1, 0, 0];
		req.extend_from_slice(&internal.to_be_bytes());
		req.extend_from_slice(&internal.to_be_bytes());
		req.extend_from_slice(&LEASE_SECS.to_be_bytes());
		let Some(r) = natpmp_request(gateway, &req, 1).await else { continue };
		if r.len() < 16 || u16::from_be_bytes([r[2], r[3]]) != 0 {
			tracing::info!(%gateway, result = ?r.get(2..4).map(|b| u16::from_be_bytes([b[0], b[1]])), "port mapping: NAT-PMP refused");
			continue;
		}
		let external = u16::from_be_bytes([r[10], r[11]]);
		if external == 0 {
			continue;
		}
		return Some(Mapping::NatPmp { gateway, internal, external });
	}
	None
}

async fn natpmp_external_ip(gateway: Ipv4Addr) -> Option<Ipv4Addr> {
	let r = natpmp_request(gateway, &[0, 0], 0).await?;
	(r.len() >= 12 && u16::from_be_bytes([r[2], r[3]]) == 0).then(|| Ipv4Addr::new(r[8], r[9], r[10], r[11]))
}

/// One NAT-PMP request/response, with a shortened version of the RFC's retransmit schedule
/// (two tries).
async fn natpmp_request(gateway: Ipv4Addr, req: &[u8], op: u8) -> Option<Vec<u8>> {
	let s = UdpSocket::bind("0.0.0.0:0").await.ok()?;
	s.connect(SocketAddr::new(gateway.into(), NATPMP_PORT)).await.ok()?;
	// A router that speaks NAT-PMP answers within a few ms on the LAN.
	for wait_ms in [100u64, 200] {
		s.send(req).await.ok()?;
		let mut buf = [0u8; 64];
		let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
		loop {
			let left = deadline.saturating_duration_since(tokio::time::Instant::now());
			if left.is_zero() {
				break;
			}
			match timeout(left, s.recv(&mut buf)).await {
				Ok(Ok(n)) if n >= 4 && buf[0] == 0 && buf[1] == 128 + op => return Some(buf[..n].to_vec()),
				Ok(Ok(_)) => continue,
				// ICMP port unreachable (no NAT-PMP there) surfaces as a reset on Windows.
				Ok(Err(_)) => return None,
				Err(_) => break,
			}
		}
	}
	None
}

async fn natpmp_delete(gateway: Ipv4Addr, internal: u16) {
	// A mapping request with lease 0 and external port 0 deletes it.
	let mut req = vec![0u8, 1, 0, 0];
	req.extend_from_slice(&internal.to_be_bytes());
	req.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
	let _ = natpmp_request(gateway, &req, 1).await;
}

/// 100.64.0.0/10 — the router's own WAN address is a carrier NAT address, not public.
fn is_cgnat(ip: Ipv4Addr) -> bool {
	let o = ip.octets();
	o[0] == 100 && (64..128).contains(&o[1])
}

// ---------------------------------------------------------------- UPnP IGD

const WAN_SERVICES: [&str; 3] = [
	"urn:schemas-upnp-org:service:WANIPConnection:2",
	"urn:schemas-upnp-org:service:WANIPConnection:1",
	"urn:schemas-upnp-org:service:WANPPPConnection:1",
];

async fn upnp_map(internal: u16, local_ip: Ipv4Addr) -> Option<Mapping> {
	let m = upnp_map_quiet(internal, local_ip).await?;
	if let Mapping::Upnp { control, service, external, permanent, .. } = &m {
		let wan = upnp_external_ip(control, service).await;
		tracing::info!(
			port = internal,
			external,
			permanent,
			router_wan = ?wan,
			behind_carrier_nat = wan.is_some_and(is_cgnat),
			"port mapping: UPnP mapped the node's UDP port on the router"
		);
	}
	Some(m)
}

async fn upnp_map_quiet(internal: u16, local_ip: Ipv4Addr) -> Option<Mapping> {
	let location = ssdp_discover(local_ip).await?;
	let (status, desc) = http("GET", &location, &[], "").await?;
	if status != 200 {
		return None;
	}
	let (service, control) = wan_service(&desc, &location)?;
	for (lease, permanent) in [(LEASE_SECS, false), (0, true)] {
		let args = format!(
			"<NewRemoteHost></NewRemoteHost><NewExternalPort>{internal}</NewExternalPort>\
			 <NewProtocol>UDP</NewProtocol><NewInternalPort>{internal}</NewInternalPort>\
			 <NewInternalClient>{local_ip}</NewInternalClient><NewEnabled>1</NewEnabled>\
			 <NewPortMappingDescription>{UPNP_DESCRIPTION}</NewPortMappingDescription>\
			 <NewLeaseDuration>{lease}</NewLeaseDuration>"
		);
		let (status, body) = soap(&control, &service, "AddPortMapping", &args).await?;
		if status == 200 {
			return Some(Mapping::Upnp {
				control,
				service,
				internal,
				external: internal,
				permanent,
			});
		}
		// 725 OnlyPermanentLeasesSupported → retry with lease 0; anything else is final
		// (718 = the port is mapped to another machine).
		let code = tag(&body, "errorCode").unwrap_or_default();
		if code != "725" {
			tracing::info!(status, code, port = internal, "port mapping: UPnP AddPortMapping refused");
			return None;
		}
	}
	None
}

async fn upnp_external_ip(control: &str, service: &str) -> Option<Ipv4Addr> {
	let (status, body) = soap(control, service, "GetExternalIPAddress", "").await?;
	(status == 200).then(|| tag(&body, "NewExternalIPAddress")?.parse().ok())?
}

async fn upnp_delete(control: &str, service: &str, external: u16) {
	let args = format!(
		"<NewRemoteHost></NewRemoteHost><NewExternalPort>{external}</NewExternalPort><NewProtocol>UDP</NewProtocol>"
	);
	let _ = soap(control, service, "DeletePortMapping", &args).await;
}

async fn remove(m: &Mapping) {
	match m {
		Mapping::NatPmp { gateway, internal, .. } => natpmp_delete(*gateway, *internal).await,
		Mapping::Upnp { control, service, external, .. } => upnp_delete(control, service, *external).await,
	}
	tracing::info!(external = m.external(), "port mapping: removed");
}

/// SSDP M-SEARCH for an Internet Gateway Device on the interface `local_ip` belongs to;
/// returns the first answer's description URL.
async fn ssdp_discover(local_ip: Ipv4Addr) -> Option<String> {
	use socket2::{Domain, Protocol, Socket, Type};
	let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
	let _ = s.set_multicast_if_v4(&local_ip);
	s.bind(&SocketAddr::new(local_ip.into(), 0).into()).ok()?;
	s.set_nonblocking(true).ok()?;
	let s = UdpSocket::from_std(s.into()).ok()?;
	let group: SocketAddr = "239.255.255.250:1900".parse().ok()?;
	for st in [
		"urn:schemas-upnp-org:device:InternetGatewayDevice:1",
		"urn:schemas-upnp-org:device:InternetGatewayDevice:2",
		WAN_SERVICES[1],
	] {
		let msg = format!(
			"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n"
		);
		let _ = s.send_to(msg.as_bytes(), group).await;
	}
	let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
	let mut buf = [0u8; 2048];
	loop {
		let left = deadline.saturating_duration_since(tokio::time::Instant::now());
		let Ok(Ok((n, _))) = timeout(left, s.recv_from(&mut buf)).await else {
			return None;
		};
		let text = String::from_utf8_lossy(&buf[..n]);
		let location = text.lines().find_map(|l| {
			let (k, v) = l.split_once(':')?;
			k.trim().eq_ignore_ascii_case("location").then(|| v.trim().to_string())
		});
		if let Some(loc) = location.filter(|l| l.starts_with("http://")) {
			return Some(loc);
		}
	}
}

/// The WAN connection service's type and absolute control URL from a device description.
fn wan_service(desc: &str, location: &str) -> Option<(String, String)> {
	for want in WAN_SERVICES {
		for block in desc.split("<service>").skip(1) {
			let block = block.split("</service>").next().unwrap_or_default();
			if tag(block, "serviceType").as_deref() == Some(want) {
				let control = tag(block, "controlURL")?;
				return Some((want.to_string(), absolute_url(&control, desc, location)?));
			}
		}
	}
	None
}

fn absolute_url(url: &str, desc: &str, location: &str) -> Option<String> {
	if url.starts_with("http://") {
		return Some(url.to_string());
	}
	let base = tag(desc, "URLBase").filter(|b| b.starts_with("http://")).unwrap_or_else(|| location.to_string());
	let (host, port, _) = split_url(&base)?;
	let path = if url.starts_with('/') { url.to_string() } else { format!("/{url}") };
	Some(format!("http://{host}:{port}{path}"))
}

/// `http://host[:port]/path` → (host, port, path).
fn split_url(url: &str) -> Option<(String, u16, String)> {
	let rest = url.strip_prefix("http://")?;
	let (hostport, path) = match rest.find('/') {
		Some(i) => (&rest[..i], rest[i..].to_string()),
		None => (rest, "/".to_string()),
	};
	let (host, port) = match hostport.rsplit_once(':') {
		Some((h, p)) => (h.to_string(), p.parse().ok()?),
		None => (hostport.to_string(), 80),
	};
	Some((host, port, path))
}

/// Text of the first `<name>…</name>` element (namespace prefixes on the tag are not used by
/// the elements read here).
fn tag(s: &str, name: &str) -> Option<String> {
	let open = format!("<{name}>");
	let start = s.find(&open)? + open.len();
	let end = s[start..].find(&format!("</{name}>"))? + start;
	Some(s[start..end].trim().to_string())
}

async fn soap(control: &str, service: &str, action: &str, args: &str) -> Option<(u16, String)> {
	let body = format!(
		"<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
		 s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
		 <u:{action} xmlns:u=\"{service}\">{args}</u:{action}></s:Body></s:Envelope>\r\n"
	);
	let action_hdr = format!("\"{service}#{action}\"");
	http(
		"POST",
		control,
		&[("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPAction", &action_hdr)],
		&body,
	)
	.await
}

/// A minimal HTTP/1.1 client for the router's description + SOAP control endpoints
/// (`Connection: close`, body read to EOF, chunked bodies decoded). `None` on any I/O error.
async fn http(method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> Option<(u16, String)> {
	let (host, port, path) = split_url(url)?;
	let io = async {
		let s = TcpStream::connect((host.as_str(), port)).await.ok()?;
		let mut req = format!(
			"{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
			body.len()
		);
		for (k, v) in headers {
			req.push_str(&format!("{k}: {v}\r\n"));
		}
		req.push_str("\r\n");
		req.push_str(body);
		// (tokio's `io-util` extension traits aren't enabled in this workspace — use the
		// stream's own readiness + try_read/try_write.)
		let mut out = req.as_bytes();
		while !out.is_empty() {
			s.writable().await.ok()?;
			match s.try_write(out) {
				Ok(n) => out = &out[n..],
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
				Err(_) => return None,
			}
		}
		let mut raw = Vec::new();
		let mut buf = [0u8; 8192];
		loop {
			s.readable().await.ok()?;
			match s.try_read(&mut buf) {
				Ok(0) => break,
				Ok(n) => raw.extend_from_slice(&buf[..n]),
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
				Err(_) if !raw.is_empty() => break, // a reset after the body: keep what came
				Err(_) => return None,
			}
			if raw.len() > 512 * 1024 {
				break;
			}
		}
		Some(raw)
	};
	let raw = timeout(Duration::from_secs(3), io).await.ok()??;
	let text = String::from_utf8_lossy(&raw).into_owned();
	let (head, rest) = text.split_once("\r\n\r\n")?;
	let status = head.split_whitespace().nth(1)?.parse().ok()?;
	let chunked = head
		.lines()
		.any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked"));
	Some((status, if chunked { dechunk(rest) } else { rest.to_string() }))
}

fn dechunk(mut s: &str) -> String {
	let mut out = String::new();
	while let Some((size, rest)) = s.split_once("\r\n") {
		let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else { break };
		if n == 0 || rest.len() < n {
			break;
		}
		out.push_str(&rest[..n]);
		s = rest[n..].strip_prefix("\r\n").unwrap_or(&rest[n..]);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_the_wan_control_url_in_a_device_description() {
		let desc = "<root><URLBase>http://192.168.1.1:1900</URLBase><device><serviceList>\
			<service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
			<controlURL>/ctl/L3F</controlURL></service>\
			<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
			<controlURL>/ctl/IPConn</controlURL></service></serviceList></device></root>";
		let (svc, ctl) = wan_service(desc, "http://192.168.1.1:1900/rootDesc.xml").unwrap();
		assert_eq!(svc, "urn:schemas-upnp-org:service:WANIPConnection:1");
		assert_eq!(ctl, "http://192.168.1.1:1900/ctl/IPConn");
	}

	#[test]
	fn relative_control_urls_resolve_against_the_location() {
		let desc = "<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
			<controlURL>upnp/control/WANPPPConn1</controlURL></service>";
		let (_, ctl) = wan_service(desc, "http://10.0.0.138:49152/desc.xml").unwrap();
		assert_eq!(ctl, "http://10.0.0.138:49152/upnp/control/WANPPPConn1");
	}

	#[test]
	fn chunked_bodies_decode() {
		assert_eq!(dechunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), "hello world");
	}

	#[test]
	fn carrier_nat_addresses_are_recognised() {
		assert!(is_cgnat(Ipv4Addr::new(100, 64, 0, 1)));
		assert!(is_cgnat(Ipv4Addr::new(100, 127, 255, 254)));
		assert!(!is_cgnat(Ipv4Addr::new(203, 0, 113, 7))); // TEST-NET-3, a public-looking address
	}

	#[test]
	fn only_the_current_owner_releases_a_port() {
		let old = claim(40_001);
		let new = claim(40_001); // the re-bound node maps the same port
		assert!(!release(40_001, old), "the old node must not delete the new mapping");
		assert!(release(40_001, new));
	}
}
