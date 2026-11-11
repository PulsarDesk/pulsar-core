//! Plug the USB/IP-exported device into the local USB stack with the `usbip` CLI.
//!
//! Ported from ZerkSense5 (MIT, florinp93), `Core/Tools.cs` (`Usbip`).
//!
//! - **Windows:** usbip-win2 (`usbip.exe`, looked up as `%ProgramFiles%\USBip\usbip.exe`,
//!   then on `PATH`). Needs an elevated process.
//! - **Linux:** the kernel's `usbip` tool with the `vhci-hcd` module loaded
//!   (`modprobe vhci-hcd`); needs root (or `CAP_SYS_ADMIN` via a privileged helper).
//!
//! `usbip [--tcp-port N] attach -r 127.0.0.1 -b 1-1`, then `usbip detach -p <port>`.
//! `--tcp-port` is omitted for the standard port 3240.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::usbip::{trace, BUS_ID};

const STANDARD_PORT: u16 = 3240;

#[cfg(windows)]
const EXE: &str = "usbip.exe";
#[cfg(not(windows))]
const EXE: &str = "usbip";

/// Find the usbip tool: `explicit` if given, else the usbip-win2 install dir
/// (Windows), else `PATH`.
pub fn locate(explicit: Option<&Path>) -> io::Result<PathBuf> {
	if let Some(p) = explicit {
		return if p.is_file() {
			Ok(p.to_path_buf())
		} else {
			Err(missing(&format!("{} does not exist", p.display())))
		};
	}
	#[cfg(windows)]
	if let Some(pf) = std::env::var_os("ProgramFiles") {
		let p = Path::new(&pf).join("USBip").join(EXE);
		if p.is_file() {
			return Ok(p);
		}
	}
	if let Some(path) = std::env::var_os("PATH") {
		for dir in std::env::split_paths(&path) {
			let p = dir.join(EXE);
			if p.is_file() {
				return Ok(p);
			}
		}
	}
	Err(missing(&format!("{EXE} not found")))
}

fn missing(why: &str) -> io::Error {
	#[cfg(windows)]
	let hint = "install usbip-win2 (https://github.com/vadimgrn/usbip-win2)";
	#[cfg(not(windows))]
	let hint = "install the usbip tools (linux-tools / usbip package) and load vhci-hcd";
	io::Error::new(io::ErrorKind::NotFound, format!("usbip: {why}; {hint}"))
}

fn run(exe: &Path, args: &[String]) -> io::Result<Output> {
	let mut cmd = Command::new(exe);
	cmd.args(args);
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const CREATE_NO_WINDOW: u32 = 0x0800_0000;
		cmd.creation_flags(CREATE_NO_WINDOW);
	}
	cmd.output()
}

fn text(o: &Output) -> String {
	let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
	s.push('\n');
	s.push_str(&String::from_utf8_lossy(&o.stderr));
	s
}

/// The first `port <N>` in `usbip attach` output (case-insensitive), as usbip-win2
/// prints "succesfully attached to port 1".
pub fn parse_attach_port(out: &str) -> Option<u32> {
	let lower = out.to_ascii_lowercase();
	let mut rest = lower.as_str();
	while let Some(i) = rest.find("port") {
		let after = &rest[i + 4..];
		let trimmed = after.trim_start();
		if trimmed.len() < after.len() {
			let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
			if let Ok(n) = digits.parse() {
				return Some(n);
			}
		}
		rest = after;
	}
	None
}

/// The vhci port of our import in Linux `usbip port` output: the `Port NN:` block that
/// names `127.0.0.1:<tcp_port>/1-1`.
pub fn parse_port_list(out: &str, tcp_port: u16) -> Option<u32> {
	let needle = format!("127.0.0.1:{tcp_port}/{BUS_ID}");
	let mut current = None;
	for line in out.lines() {
		let t = line.trim_start();
		if let Some(rest) = t.strip_prefix("Port ") {
			current = rest.split(':').next().and_then(|n| n.trim().parse().ok());
		} else if t.contains(&needle) {
			if current.is_some() {
				return current;
			}
		}
	}
	None
}

/// Every `Port NN:` in `usbip port` output (both usbip-win2 and Linux print that).
pub fn parse_imported_ports(out: &str) -> Vec<u32> {
	out.lines()
		.filter_map(|l| l.trim_start().strip_prefix("Port "))
		.filter_map(|rest| rest.split(':').next()?.trim().parse().ok())
		.collect()
}

/// The vhci ports currently imported, from `usbip port`.
pub fn imported_ports(exe: &Path) -> io::Result<Vec<u32>> {
	let o = run(exe, &["port".into()])?;
	Ok(parse_imported_ports(&text(&o)))
}

fn global_args(tcp_port: u16) -> Vec<String> {
	if tcp_port == STANDARD_PORT {
		vec![]
	} else {
		vec!["--tcp-port".into(), tcp_port.to_string()]
	}
}

/// Cancel usbip-win2's automatic re-attach attempts to our server
/// (`usbip --tcp-port N attach -r 127.0.0.1 -b 1-1 --stop`).
///
/// usbip-win2 re-attaches on ANY network error after a successful attach (the
/// maintainer, issue #175): a server-side socket close starts attempts after 30 s,
/// backing off to 8 min, 20 times (~2 h). Only `attach --stop` or `detach -p` while the
/// device is still plugged prevent them. Linux usbip has no such attempts; there the
/// command fails harmlessly.
pub fn stop_attach_attempts(exe: &Path, tcp_port: u16) {
	if !cfg!(windows) {
		return;
	}
	let mut args = global_args(tcp_port);
	args.extend(["attach", "-r", "127.0.0.1", "-b", BUS_ID, "--stop"].map(String::from));
	match run(exe, &args) {
		Ok(o) => trace!(
			"usbip {} -> {}: {}",
			args.join(" "),
			o.status,
			text(&o).trim()
		),
		Err(e) => trace!("usbip {}: {e}", args.join(" ")),
	}
}

/// A live `usbip attach`; [`Attachment::detach`] (or drop) unplugs it.
#[derive(Debug)]
pub struct Attachment {
	exe: PathBuf,
	tcp_port: u16,
	vhci_port: Option<u32>,
}

impl Attachment {
	/// The vhci port the device was attached to.
	pub fn vhci_port(&self) -> Option<u32> {
		self.vhci_port
	}

	/// The usbip tool this attachment uses.
	pub fn exe(&self) -> &Path {
		&self.exe
	}

	/// `usbip detach -p <port>`. Returns whether the tool reported success. Safe to call
	/// more than once (later calls do nothing).
	pub fn detach(&mut self) -> bool {
		let Some(p) = self.vhci_port.take() else {
			return false;
		};
		match run(&self.exe, &["detach".into(), "-p".into(), p.to_string()]) {
			Ok(o) if o.status.success() => {
				trace!("usbip: detached port {p}: {}", text(&o).trim());
				true
			}
			Ok(o) => {
				trace!("usbip detach -p {p} failed: {}", text(&o).trim());
				false
			}
			Err(e) => {
				trace!("usbip detach -p {p}: {e}");
				false
			}
		}
	}

	/// See [`stop_attach_attempts`].
	pub fn stop_reattach(&self) {
		stop_attach_attempts(&self.exe, self.tcp_port);
	}

	fn do_detach(&mut self) {
		if self.vhci_port.is_some() {
			self.detach();
			self.stop_reattach();
		}
	}
}

impl Drop for Attachment {
	fn drop(&mut self) {
		self.do_detach();
	}
}

/// Run `usbip attach` against our server on 127.0.0.1:`tcp_port`.
pub fn attach(exe: &Path, tcp_port: u16) -> io::Result<Attachment> {
	let mut args = global_args(tcp_port);
	args.extend(["attach", "-r", "127.0.0.1", "-b", BUS_ID].map(String::from));
	let o = run(exe, &args)?;
	trace!(
		"usbip {} -> {}: {}",
		args.join(" "),
		o.status,
		text(&o).trim()
	);
	let out = text(&o);
	if !o.status.success() {
		return Err(io::Error::other(format!(
			"usbip attach failed ({}): {}",
			o.status,
			out.trim()
		)));
	}
	let mut vhci_port = parse_attach_port(&out);
	if vhci_port.is_none() {
		// Linux usbip prints nothing on success; ask `usbip port`.
		if let Ok(o) = run(exe, &["port".into()]) {
			vhci_port = parse_port_list(&text(&o), tcp_port);
		}
	}
	if vhci_port.is_none() {
		tracing::warn!(
			"usbip attach: no port in output, detach will be skipped: {}",
			out.trim()
		);
	}
	Ok(Attachment {
		exe: exe.to_path_buf(),
		tcp_port,
		vhci_port,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_windows_attach_output() {
		assert_eq!(
			parse_attach_port("succesfully attached to port 3\n"),
			Some(3)
		);
		assert_eq!(parse_attach_port("Port 12 attached"), Some(12));
		assert_eq!(parse_attach_port("usbip: error: --tcp-port missing"), None);
	}

	#[test]
	fn parses_linux_port_list() {
		let out = "Imported USB devices\n====================\n\
			Port 00: <Port in Use> at Full Speed(12Mbps)\n\
			       Sony Corp. : unknown product (054c:0ce6)\n\
			       3-1 -> usbip://127.0.0.1:3240/1-1\n\
			Port 01: <Port in Use> at Full Speed(12Mbps)\n\
			       Sony Corp. : unknown product (054c:0ce6)\n\
			       3-2 -> usbip://127.0.0.1:50123/1-1\n";
		assert_eq!(parse_port_list(out, 50123), Some(1));
		assert_eq!(parse_port_list(out, 3240), Some(0));
		assert_eq!(parse_port_list(out, 4000), None);
		assert_eq!(parse_imported_ports(out), vec![0, 1]);
	}

	#[test]
	fn parses_windows_port_list() {
		let out = "Imported USB devices
====================
			Port 01: device in use at Full Speed(12Mbps)
			         Sony Corp. : DualSense wireless controller (PS5) (054c:0ce6)
			           -> usbip://127.0.0.1:50123/1-1
			           -> remote bus/dev: 001/001
";
		assert_eq!(parse_imported_ports(out), vec![1]);
		assert_eq!(parse_port_list(out, 50123), Some(1));
		assert!(parse_imported_ports(
			"Imported USB devices
====================
"
		)
		.is_empty());
	}

	#[test]
	fn missing_tool_is_a_clear_error() {
		let e = locate(Some(Path::new("Z:/definitely/not/usbip.exe"))).unwrap_err();
		assert_eq!(e.kind(), io::ErrorKind::NotFound);
		assert!(e.to_string().contains("usbip"));
	}
}
