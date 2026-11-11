//! Leftover virtual pads: the record of every pad this host plugged in, and the pure
//! decisions about which leftovers to remove. The Windows side that reads devnodes, probes
//! processes and removes devices is `pad_cleanup.rs`.
//!
//! Why: a Pulsar host killed without unwinding (crash, `taskkill /f`, power loss of the
//! session) never runs the pads' `Drop`. A Duo controller then outlives it, and in one case
//! (2026-11-12) its HID child even stayed present after its ROOT devnode was gone. Steam and
//! games picked that dead pad first, so the seat's real pad did nothing. A USB/IP pad leaves
//! usbip-win2 re-attach attempts behind (~2 h) that can grab a later server on the same port.
//!
//! Two rules, both run at host start and (throttled) before a new pad is created:
//! - **Orphan HID** ([`hid_is_orphan`]): a present `HID\…&DUOCONTROLLER&…` devnode whose
//!   parent is gone, not present or being removed. Dead for every seat, whoever made it.
//! - **Owner gone** ([`plan_reap`]): a recorded pad whose creating process is no longer
//!   running (pid + process start time, so a reused pid doesn't keep it alive).
//!
//! Duo controllers that Pulsar did not create are never touched except by the first rule.

use serde::{Deserialize, Serialize};

/// The record file's current format.
pub const VERSION: u32 = 1;

/// One virtual pad a Pulsar process plugged in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PadRecord {
	/// A controller made through Duo's `DuoController.dll`: its ROOT devnode instance id.
	Duo {
		instance_id: String,
		#[serde(flatten)]
		owner: Owner,
	},
	/// A USB/IP DualSense: our server's TCP port on 127.0.0.1 and the vhci port it got.
	Usbip {
		tcp_port: u16,
		vhci_port: Option<u32>,
		#[serde(flatten)]
		owner: Owner,
	},
}

impl PadRecord {
	pub fn owner(&self) -> &Owner {
		match self {
			Self::Duo { owner, .. } | Self::Usbip { owner, .. } => owner,
		}
	}

	/// Same device (ignores the owner): same Duo instance id (case-insensitive) or the
	/// same USB/IP server port.
	pub fn same_device(&self, other: &PadRecord) -> bool {
		match (self, other) {
			(Self::Duo { instance_id: a, .. }, Self::Duo { instance_id: b, .. }) => {
				a.eq_ignore_ascii_case(b)
			}
			(Self::Usbip { tcp_port: a, .. }, Self::Usbip { tcp_port: b, .. }) => a == b,
			_ => false,
		}
	}
}

/// The process that created a pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
	pub pid: u32,
	/// Process creation time (Windows FILETIME, 100 ns since 1601); 0 = unknown.
	#[serde(default)]
	pub started: u64,
}

/// The record file (`%ProgramData%\Pulsar\duo-pads.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
	#[serde(default)]
	pub version: u32,
	#[serde(default)]
	pub pads: Vec<PadRecord>,
}

impl Registry {
	/// Parse the file; anything unreadable is an empty registry (a corrupt file must not
	/// block pad creation; the worst case is one leftover we can't attribute).
	pub fn parse(raw: &str) -> Self {
		serde_json::from_str(raw).unwrap_or_default()
	}

	pub fn to_json(&self) -> String {
		let r = Registry {
			version: VERSION,
			pads: self.pads.clone(),
		};
		serde_json::to_string_pretty(&r).unwrap_or_default()
	}

	/// Add `rec`, replacing an older record of the same device (a devnode/port reused by
	/// a new owner).
	pub fn add(&mut self, rec: PadRecord) {
		self.pads.retain(|p| !p.same_device(&rec));
		self.pads.push(rec);
	}

	/// Drop the record of `rec`'s device if `rec`'s owner still holds it.
	pub fn remove(&mut self, rec: &PadRecord) {
		self.pads
			.retain(|p| !(p.same_device(rec) && p.owner() == rec.owner()));
	}
}

/// What a process probe found for a pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
	/// No such process, or it has exited.
	Gone,
	/// Running; its start time when it could be read.
	Running(Option<u64>),
}

/// Whether the pad's creator is gone. A running process with a DIFFERENT start time is a
/// reused pid, so the creator is gone too. Unknown start times on either side count as
/// alive (never remove a live process's pad on a guess).
pub fn owner_gone(owner: &Owner, now: ProcState) -> bool {
	match now {
		ProcState::Gone => true,
		ProcState::Running(Some(t)) => owner.started != 0 && t != owner.started,
		ProcState::Running(None) => false,
	}
}

/// Split the records into `(keep, reap)`: reap those whose owner is gone. `probe` asks the
/// OS about a pid (called once per distinct owner).
pub fn plan_reap(
	pads: &[PadRecord],
	mut probe: impl FnMut(u32) -> ProcState,
) -> (Vec<PadRecord>, Vec<PadRecord>) {
	let mut seen: Vec<(u32, ProcState)> = Vec::new();
	let (mut keep, mut reap) = (Vec::new(), Vec::new());
	for p in pads {
		let pid = p.owner().pid;
		let st = match seen.iter().find(|(q, _)| *q == pid) {
			Some((_, s)) => *s,
			None => {
				let s = probe(pid);
				seen.push((pid, s));
				s
			}
		};
		if owner_gone(p.owner(), st) {
			reap.push(p.clone());
		} else {
			keep.push(p.clone());
		}
	}
	(keep, reap)
}

/// A Duo controller's ROOT devnode instance id (`ROOT\VID_…&PID_…&DUOCONTROLLER&…\NNNN`).
/// Records naming anything else are never acted on (a planted record can't make the
/// elevated host remove an arbitrary device).
pub fn is_duo_root_instance(id: &str) -> bool {
	let u = id.to_ascii_uppercase();
	u.starts_with(r"ROOT\") && u.contains("&DUOCONTROLLER&")
}

/// A Duo controller's HID child devnode (`HID\VID_…&PID_…&DUOCONTROLLER&…\…`).
pub fn is_duo_hid_instance(id: &str) -> bool {
	let u = id.to_ascii_uppercase();
	u.starts_with(r"HID\") && u.contains("&DUOCONTROLLER&")
}

/// The parent of a Duo HID devnode, as CfgMgr32 sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParentState {
	/// `CM_Get_Parent` failed: no parent devnode.
	Missing,
	/// The parent is the tree root (`HTREE\ROOT\0`): the controller's ROOT devnode is gone.
	TreeRoot,
	/// The parent's devnode exists but `CM_Get_DevNode_Status` says it isn't there.
	NotPresent,
	/// Present, with this problem code (0 = none) and instance id.
	Present { id: String, problem: u32 },
}

/// Problem codes of a parent that is on its way out (CM_PROB_WILL_BE_REMOVED,
/// CM_PROB_DEVICE_NOT_THERE, CM_PROB_PHANTOM, CM_PROB_HELD_FOR_EJECT).
const DEAD_PARENT_PROBLEMS: [u32; 4] = [21, 24, 45, 47];

/// Whether a present Duo HID devnode is an orphan, dead for every seat: its controller's
/// ROOT devnode is missing, not present or being removed. A present parent that isn't a
/// Duo ROOT devnode is NOT an orphan (an unknown device tree: leave it alone).
pub fn hid_is_orphan(hid_id: &str, parent: &ParentState) -> bool {
	if !is_duo_hid_instance(hid_id) {
		return false;
	}
	match parent {
		ParentState::Missing | ParentState::TreeRoot | ParentState::NotPresent => true,
		ParentState::Present { id, problem } => {
			is_duo_root_instance(id) && DEAD_PARENT_PROBLEMS.contains(problem)
		}
	}
}

/// What to do about a dead owner's USB/IP pad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbipReap {
	/// `usbip detach -p N`: the vhci port still holds an import of that server.
	pub detach: Option<u32>,
	/// `usbip attach --stop` for the server port (cancels usbip-win2's re-attach attempts).
	pub stop_reattach: bool,
}

/// `imported`: `(vhci port, server tcp port)` of every 127.0.0.1 import in `usbip port`.
/// `live`: records of owners that are still running. A port another live Pulsar uses now
/// (reused ephemeral port) is left alone.
pub fn usbip_reap(dead: &PadRecord, live: &[PadRecord], imported: &[(u32, u16)]) -> UsbipReap {
	let PadRecord::Usbip {
		tcp_port,
		vhci_port,
		..
	} = dead
	else {
		return UsbipReap::default();
	};
	if live.iter().any(|l| l.same_device(dead)) {
		return UsbipReap::default();
	}
	let detach = imported
		.iter()
		.find(|(port, tcp)| tcp == tcp_port && vhci_port.is_none_or(|v| v == *port))
		.map(|(port, _)| *port);
	UsbipReap {
		detach,
		stop_reattach: true,
	}
}

/// Pick the ROOT devnode a new Duo controller got: the only Duo ROOT id in `now` that is
/// in neither `before` nor `taken` (recorded by anyone, or claimed by a pad of this
/// process). `None` while it hasn't appeared, and also when more than one appeared (another
/// seat created one at the same moment: recording the wrong one could later remove a live
/// pad of that seat).
pub fn pick_new_root(before: &[String], now: &[String], taken: &[String]) -> Option<String> {
	let has = |v: &[String], x: &str| v.iter().any(|b| b.eq_ignore_ascii_case(x));
	let mut fresh = now
		.iter()
		.filter(|p| is_duo_root_instance(p) && !has(before, p) && !has(taken, p));
	let first = fresh.next()?;
	if fresh.next().is_some() {
		return None;
	}
	Some(first.clone())
}

#[cfg(test)]
mod tests {
	use super::*;

	const HID: &str =
		r"HID\VID_054C&PID_0CE6&DUOCONTROLLER&00722B6A0000000000009C1000000001\1&1D375D0D&0&0000";
	const ROOT: &str =
		r"ROOT\VID_054C&PID_0CE6&DUOCONTROLLER&00722B6A0000000000009C1000000001\0000";

	fn duo(id: &str, pid: u32, started: u64) -> PadRecord {
		PadRecord::Duo {
			instance_id: id.into(),
			owner: Owner { pid, started },
		}
	}

	fn usbip(tcp: u16, vhci: Option<u32>, pid: u32) -> PadRecord {
		PadRecord::Usbip {
			tcp_port: tcp,
			vhci_port: vhci,
			owner: Owner { pid, started: 7 },
		}
	}

	#[test]
	fn orphan_decision() {
		// The home-PC case: HID child present, ROOT parent gone.
		assert!(hid_is_orphan(HID, &ParentState::Missing));
		assert!(hid_is_orphan(HID, &ParentState::TreeRoot));
		assert!(hid_is_orphan(HID, &ParentState::NotPresent));
		assert!(hid_is_orphan(
			HID,
			&ParentState::Present {
				id: ROOT.into(),
				problem: 21
			}
		));
		// A healthy pad of any seat.
		assert!(!hid_is_orphan(
			HID,
			&ParentState::Present {
				id: ROOT.into(),
				problem: 0
			}
		));
		// Unknown tree: a present non-Duo parent, even with a problem.
		assert!(!hid_is_orphan(
			HID,
			&ParentState::Present {
				id: r"USB\VID_054C&PID_0CE6\1".into(),
				problem: 24
			}
		));
		// Not a Duo HID devnode (a real DualSense, a ViGEm pad).
		assert!(!hid_is_orphan(
			r"HID\VID_054C&PID_0CE6&MI_03\7&1",
			&ParentState::Missing
		));
		assert!(
			!hid_is_orphan(ROOT, &ParentState::Missing),
			"ROOT is not a HID child"
		);
	}

	#[test]
	fn instance_filters() {
		assert!(is_duo_root_instance(ROOT));
		assert!(is_duo_root_instance(&ROOT.to_lowercase()));
		assert!(!is_duo_root_instance(HID));
		assert!(!is_duo_root_instance(r"ROOT\SYSTEM\0001"));
		assert!(!is_duo_root_instance(
			r"USB\VID_054C&PID_0CE6&DUOCONTROLLER&1\0"
		));
		assert!(is_duo_hid_instance(HID));
		assert!(!is_duo_hid_instance(ROOT));
	}

	#[test]
	fn owner_liveness() {
		let o = Owner {
			pid: 10,
			started: 500,
		};
		assert!(owner_gone(&o, ProcState::Gone));
		assert!(!owner_gone(&o, ProcState::Running(Some(500))));
		assert!(owner_gone(&o, ProcState::Running(Some(501))), "pid reused");
		assert!(!owner_gone(&o, ProcState::Running(None)), "unknown = alive");
		let unknown = Owner {
			pid: 10,
			started: 0,
		};
		assert!(!owner_gone(&unknown, ProcState::Running(Some(501))));
		assert!(owner_gone(&unknown, ProcState::Gone));
	}

	#[test]
	fn reap_plan_probes_each_pid_once() {
		let pads = vec![
			duo(ROOT, 1, 100),
			duo(r"ROOT\VID_045E&PID_02FF&DUOCONTROLLER&2\0000", 2, 200),
			usbip(50000, Some(3), 1),
			duo(r"ROOT\VID_054C&PID_05C4&DUOCONTROLLER&3\0000", 3, 300),
		];
		let mut calls = Vec::new();
		let (keep, reap) = plan_reap(&pads, |pid| {
			calls.push(pid);
			match pid {
				1 => ProcState::Gone,
				2 => ProcState::Running(Some(200)),
				_ => ProcState::Running(Some(999)),
			}
		});
		assert_eq!(calls, vec![1, 2, 3]);
		assert_eq!(keep, vec![pads[1].clone()]);
		assert_eq!(
			reap,
			vec![pads[0].clone(), pads[2].clone(), pads[3].clone()]
		);
	}

	#[test]
	fn registry_round_trip_and_updates() {
		let mut r = Registry::default();
		r.add(duo(ROOT, 1, 100));
		r.add(usbip(50000, Some(2), 1));
		// Same devnode, new owner: replaces.
		r.add(duo(&ROOT.to_lowercase(), 2, 200));
		assert_eq!(r.pads.len(), 2);
		let back = Registry::parse(&r.to_json());
		assert_eq!(back.version, VERSION);
		assert_eq!(back.pads, r.pads);
		// The old owner's late removal doesn't drop the new owner's record.
		r.remove(&duo(ROOT, 1, 100));
		assert_eq!(r.pads.len(), 2);
		r.remove(&duo(ROOT, 2, 200));
		assert_eq!(r.pads, vec![usbip(50000, Some(2), 1)]);
		assert_eq!(Registry::parse("not json"), Registry::default());
		assert_eq!(Registry::parse(""), Registry::default());
		// The on-disk shape.
		let j = duo(ROOT, 1, 100);
		let s = serde_json::to_string(&j).unwrap();
		assert!(
			s.contains(r#""kind":"duo""#) && s.contains(r#""pid":1"#),
			"{s}"
		);
	}

	#[test]
	fn usbip_reap_decision() {
		let dead = usbip(50123, Some(1), 9);
		// Still imported on its port: detach it, then stop re-attach attempts.
		assert_eq!(
			usbip_reap(&dead, &[], &[(1, 50123), (2, 3240)]),
			UsbipReap {
				detach: Some(1),
				stop_reattach: true
			}
		);
		// Already unplugged (usbip-win2 dropped it when our server died): only stop.
		assert_eq!(
			usbip_reap(&dead, &[], &[(2, 3240)]),
			UsbipReap {
				detach: None,
				stop_reattach: true
			}
		);
		// Port number reused by a re-attach on another vhci port: still ours to detach.
		let unknown_port = usbip(50123, None, 9);
		assert_eq!(
			usbip_reap(&unknown_port, &[], &[(4, 50123)]).detach,
			Some(4)
		);
		// Another live Pulsar uses that server port now: hands off.
		assert_eq!(
			usbip_reap(&dead, &[usbip(50123, Some(1), 10)], &[(1, 50123)]),
			UsbipReap::default()
		);
		assert_eq!(usbip_reap(&duo(ROOT, 1, 1), &[], &[]), UsbipReap::default());
	}

	#[test]
	fn new_root_pick() {
		let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
		let a = r"ROOT\VID_054C&PID_0CE6&DUOCONTROLLER&A\0000";
		let b = r"ROOT\VID_045E&PID_02FF&DUOCONTROLLER&B\0000";
		let c = r"ROOT\VID_054C&PID_05C4&DUOCONTROLLER&C\0000";
		let before = s(&[a]);
		assert_eq!(pick_new_root(&before, &s(&[a]), &[]), None, "not up yet");
		assert_eq!(pick_new_root(&before, &s(&[a, b]), &[]).as_deref(), Some(b));
		assert_eq!(
			pick_new_root(&before, &s(&[a, b, c]), &[]),
			None,
			"two appeared: ambiguous, record nothing"
		);
		assert_eq!(
			pick_new_root(&before, &s(&[a, b, c]), &s(&[&c.to_lowercase()])).as_deref(),
			Some(b),
			"one of them is taken"
		);
		assert_eq!(
			pick_new_root(&[], &s(&[r"ROOT\SYSTEM\0001"]), &[]),
			None,
			"not a Duo devnode"
		);
	}
}
