//! Which client session a controller's input goes to.
//!
//! A desktop client can run several sessions at once (tabs, or split-screen panes). Every
//! session reads the same local pads, so each must decide on its own whether a given pad
//! is "its" pad. The gamepad forward loop and the touchpad-as-mouse readers both use
//! [`pad_routes_to`], so a pad's buttons and its touchpad always reach the same host.

/// The routing inputs, as one session sees them at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadRoute {
	/// The deciding session's id.
	pub play: u64,
	/// The session the pad is locked to in the overlay, if any.
	pub lock_owner: Option<u64>,
	/// The focused session / pane (0 = none set yet).
	pub focused: u64,
	/// Live sessions: `Some((count, focused_is_live))`, or `None` when the session map is
	/// busy this instant (then the tab rule falls back to "forward", the old behaviour).
	pub sessions: Option<(usize, bool)>,
	/// Split-screen panes shown (<= 1 = split off).
	pub split_panes: usize,
	/// Split "press to join" is running (gaming split: unassigned pads wait for an A press).
	pub split_join_active: bool,
}

/// Whether the pad drives session `r.play`.
///
/// - **Split off (tabs)**: one session gets every pad. With several tabs only the
///   focused tab does; if the focused id is not a live session (none set yet, or it
///   closed) every session forwards, as before tabs existed. A pad lock is ignored
///   here (locks are a split-pane feature).
/// - **Split on**: a locked pad goes only to its owner pane. An unlocked pad goes to no
///   pane during "press to join", else only to the focused pane.
pub fn pad_routes_to(r: &PadRoute) -> bool {
	if r.split_panes <= 1 {
		match r.sessions {
			Some((n, focused_live)) => n <= 1 || r.focused == r.play || !focused_live,
			None => true,
		}
	} else if let Some(owner) = r.lock_owner {
		owner == r.play
	} else if r.split_join_active {
		false
	} else {
		r.focused == r.play
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn base(play: u64) -> PadRoute {
		PadRoute {
			play,
			lock_owner: None,
			focused: 1,
			sessions: Some((1, true)),
			split_panes: 1,
			split_join_active: false,
		}
	}

	#[test]
	fn single_session_gets_every_pad() {
		let mut r = base(1);
		r.focused = 0;
		r.sessions = Some((1, false));
		assert!(pad_routes_to(&r));
	}

	#[test]
	fn tabs_only_focused_tab() {
		let mut r = base(1);
		r.sessions = Some((2, true));
		assert!(pad_routes_to(&r));
		r.play = 2;
		assert!(!pad_routes_to(&r));
	}

	#[test]
	fn tabs_without_live_focus_forward_everywhere() {
		let mut r = base(2);
		r.focused = 9;
		r.sessions = Some((2, false));
		assert!(pad_routes_to(&r));
		r.sessions = None;
		assert!(pad_routes_to(&r));
	}

	#[test]
	fn split_unlocked_pad_follows_focus() {
		let mut r = base(1);
		r.split_panes = 2;
		r.sessions = Some((2, true));
		assert!(pad_routes_to(&r));
		r.play = 2;
		assert!(!pad_routes_to(&r));
	}

	#[test]
	fn split_locked_pad_goes_to_owner_only() {
		let mut r = base(1);
		r.split_panes = 2;
		r.lock_owner = Some(2);
		assert!(!pad_routes_to(&r));
		r.play = 2;
		assert!(pad_routes_to(&r));
		// The lock wins over "press to join" too.
		r.split_join_active = true;
		assert!(pad_routes_to(&r));
	}

	#[test]
	fn split_join_holds_unassigned_pads() {
		let mut r = base(1);
		r.split_panes = 3;
		r.split_join_active = true;
		assert!(!pad_routes_to(&r));
	}
}
