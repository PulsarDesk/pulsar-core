//! Keepalive RTT measurement shared by the desktop and mobile clients: the uneven Ping cadence
//! and the matcher that times the host's Pongs (which carry no id) against our Pings.
//! Extracted 2026-11-15 from the desktop hold loop without a behaviour change.

/// Keepalive/RTT ping cadence. 500 ms (was 2 s): the adaptive controller (`pulsar_core::adapt`)
/// decides on 2 s windows and needs a few RTT samples per window to see a queue building
/// (Phase 0.1) and to size the NACK repair window (Phase 0.5). Each Ping is a ~10-byte
/// datagram; the host answers every one with a Pong and refreshes its liveness timer.
pub const PING_MS: u64 = 500;
/// A Ping still unanswered this long is written off as lost. Pings go out every `PING_MS`
/// whether or not the last one was answered (see `match_pong`): on a 40 %-loss link a lost
/// Ping or Pong used to stall the RTT samples for a second.
pub const PING_LOST: std::time::Duration = std::time::Duration::from_secs(2);

/// Which unanswered Ping a Pong answers (`ages` = time since each was sent, oldest first;
/// `expected_ms` = the latest RTT sample, 0 = none yet). The Pong carries no id: take the Ping
/// whose age is nearest the latest RTT. A lost Ping (or Pong) leaves an old Ping ~`PING_MS`
/// too old, so the next Pong is timed against the newer one; an RTT that grows past `PING_MS`
/// (a filling queue) keeps matching the older one. No sample yet: the oldest. Ties go to the
/// older Ping.
pub fn match_pong(ages: &[std::time::Duration], expected_ms: f32) -> Option<usize> {
	if ages.is_empty() {
		return None;
	}
	if expected_ms <= 0.0 {
		return Some(0);
	}
	let off = |a: &std::time::Duration| (a.as_secs_f32() * 1000.0 - expected_ms).abs();
	let mut best = 0;
	for (i, a) in ages.iter().enumerate().skip(1) {
		if off(a) < off(&ages[best]) {
			best = i;
		}
	}
	Some(best)
}

/// Pongs a `PongMatcher` vote looks back over.
pub const PONG_VOTES: usize = 4;
/// A vote needs this many of the last `PONG_VOTES` Pongs behind it; with less evidence the
/// matcher falls back to `match_pong`.
pub const PONG_VOTE_MIN: i32 = 2;
/// RTT drift (ms per ms) a vote allows for since its Pong: 0.1 = 100 ms a second, more than a
/// filling queue shows, far less than a Ping gap (350+ ms) — wrong candidates still miss.
pub const PONG_DRIFT: f32 = 0.1;

/// The gap before the Ping after the `n`-th (ms): 350–650 ms, mean `PING_MS`, never periodic
/// (a golden-ratio sequence). With a fixed 500 ms cadence a Pong cannot tell its Ping from the
/// next one: a sustained 600 ms RTT and a 100 ms RTT with one lost Ping give the very same
/// arrival times, and the matcher locked onto RTT − 500 for as long as the RTT stayed over
/// 500 ms (the repair window shrank under the real RTT, the ABR read no queue). Uneven gaps make
/// only the true RTT repeat from Pong to Pong.
pub fn ping_gap_ms(n: u64) -> u64 {
	let frac = (n as f64 * 0.618_033_988_75).fract();
	PING_MS - 150 + (300.0 * frac) as u64
}

/// Times the host's Pongs against our Pings. A Pong carries no id (the wire format stays as
/// released apps speak it), so each Pong could answer any recent Ping. The RTT that keeps
/// showing up among the candidate ages of the last `PONG_VOTES` Pongs wins (with the uneven
/// `ping_gap_ms` cadence only the true one repeats); a Ping already answered, or older than the
/// last one answered, needs one vote more (it is the "the last match was off by a Ping" fix-up).
/// Without enough votes (a jittery link, the first Pongs) the latest-RTT rule of `match_pong`
/// picks among the unanswered newer Pings, as before.
pub struct PongMatcher {
	/// Recent Pings, oldest first: when each was sent and whether a Pong was matched to it. Kept
	/// for `PING_LOST` either way — the vote needs the answered ones too.
	sent: std::collections::VecDeque<(std::time::Instant, bool)>,
	/// The last `PONG_VOTES` Pongs: when each arrived and its candidate ages (ms).
	votes: std::collections::VecDeque<(std::time::Instant, Vec<f32>)>,
	last_matched: Option<std::time::Instant>,
	/// The latest RTT reading (ms; 0 = none yet).
	rtt_last: f32,
	/// Pings sent (the `ping_gap_ms` index).
	n: u64,
}

impl PongMatcher {
	pub fn new() -> Self {
		Self {
			sent: std::collections::VecDeque::with_capacity(8),
			votes: std::collections::VecDeque::with_capacity(PONG_VOTES + 1),
			last_matched: None,
			rtt_last: 0.0,
			n: 0,
		}
	}

	fn forget_old(&mut self, now: std::time::Instant) {
		while self.sent.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > PING_LOST) {
			self.sent.pop_front();
		}
	}

	/// A Ping went out at `now`; returns the gap before the next one.
	pub fn on_ping(&mut self, now: std::time::Instant) -> std::time::Duration {
		self.forget_old(now);
		self.sent.push_back((now, false));
		self.n += 1;
		std::time::Duration::from_millis(ping_gap_ms(self.n))
	}

	/// A Pong arrived at `now`: its RTT (ms), or `None` when no Ping is outstanding.
	pub fn on_pong(&mut self, now: std::time::Instant) -> Option<f32> {
		self.forget_old(now);
		if self.sent.is_empty() {
			return None;
		}
		let ages: Vec<f32> =
			self.sent.iter().map(|(t, _)| now.saturating_duration_since(*t).as_secs_f32() * 1000.0).collect();
		let last = self.last_matched;
		let fresh = |j: usize| {
			let (t, answered) = self.sent[j];
			!answered && last.is_none_or(|m| t > m)
		};
		// A vote counts within the scatter of one reading, plus the drift an RTT can show since
		// that Pong (a filling queue grows it by tens of ms per second).
		let score = |c: f32| {
			self.votes
				.iter()
				.filter(|(at, v)| {
					let tol = (0.05 * c).max(40.0) + PONG_DRIFT * now.saturating_duration_since(*at).as_secs_f32() * 1000.0;
					v.iter().any(|x| (c - x).abs() <= tol)
				})
				.count() as i32
		};
		let near = |c: f32| if self.rtt_last > 0.0 { -(c - self.rtt_last).abs() } else { c };
		// (key, fresh, nearness): ties go to a fresh Ping, then to the age nearest the latest RTT.
		let mut best: Option<(i32, bool, f32, usize)> = None;
		for (j, &c) in ages.iter().enumerate() {
			let f = fresh(j);
			let key = score(c) - i32::from(!f);
			let cand = (key, f, near(c), j);
			if best.is_none_or(|b| (cand.0, cand.1) > (b.0, b.1) || ((cand.0, cand.1) == (b.0, b.1) && cand.2 > b.2)) {
				best = Some(cand);
			}
		}
		let (key, _, _, mut j) = best?;
		if key < PONG_VOTE_MIN {
			// No consistent RTT yet: the latest-RTT rule among the unanswered newer Pings.
			let pool: Vec<usize> = (0..ages.len()).filter(|&j| fresh(j)).collect();
			let pool = if pool.is_empty() { (0..ages.len()).collect() } else { pool };
			let pool_ages: Vec<std::time::Duration> =
				pool.iter().map(|&j| std::time::Duration::from_secs_f32(ages[j] / 1000.0)).collect();
			j = pool[match_pong(&pool_ages, self.rtt_last)?];
		}
		self.sent[j].1 = true;
		self.last_matched = Some(self.sent[j].0);
		self.rtt_last = ages[j];
		if self.votes.len() >= PONG_VOTES {
			self.votes.pop_front();
		}
		self.votes.push_back((now, ages.clone()));
		Some(ages[j])
	}
}

impl Default for PongMatcher {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn pongs_match_the_right_ping() {
		let ms = Duration::from_millis;
		// Nothing outstanding: a stray Pong times nothing.
		assert_eq!(match_pong(&[], 80.0), None);
		// No sample yet: the oldest Ping.
		assert_eq!(match_pong(&[ms(120), ms(20)], 0.0), Some(0));
		// The previous Ping (or its Pong) was lost: time the Pong against the newer Ping, not
		// +500 ms against the lost one.
		assert_eq!(match_pong(&[ms(580), ms(80)], 78.0), Some(1));
		// A queue filling past PING_MS: the RTT keeps growing, the older Ping is the match.
		assert_eq!(match_pong(&[ms(560), ms(60)], 450.0), Some(0));
		// One outstanding: that one.
		assert_eq!(match_pong(&[ms(300)], 78.0), Some(0));
	}

	/// Drive a `PongMatcher` with the real ping cadence over a path whose RTT is `rtt(t_ms)`;
	/// `lost(i)` drops the round trip of Ping i. Returns (arrival ms, true RTT, reading).
	fn pong_run(secs: u64, rtt: impl Fn(u64) -> u64, lost: impl Fn(u64) -> bool) -> Vec<(u64, u64, f32)> {
		let base = std::time::Instant::now();
		let at = |ms: u64| base + Duration::from_millis(ms);
		let mut m = PongMatcher::new();
		// (time, is_pong, ping index)
		let mut events: Vec<(u64, bool, u64)> = Vec::new();
		let (mut t, mut i) = (0u64, 0u64);
		let mut gaps = PongMatcher::new();
		while t < secs * 1000 {
			events.push((t, false, i));
			if !lost(i) {
				events.push((t + rtt(t), true, i));
			}
			t += gaps.on_ping(at(0)).as_millis() as u64;
			i += 1;
		}
		events.sort();
		let mut sent_at = std::collections::HashMap::new();
		let mut out = Vec::new();
		for (t, pong, i) in events {
			if pong {
				if let Some(r) = m.on_pong(at(t)) {
					out.push((t, t - sent_at[&i], r));
				}
			} else {
				sent_at.insert(i, t);
				m.on_ping(at(t));
			}
		}
		out
	}

	#[test]
	fn ping_gaps_average_the_cadence_and_never_repeat() {
		let gaps: Vec<u64> = (1..=40).map(ping_gap_ms).collect();
		assert!(gaps.iter().all(|g| (350..=650).contains(g)), "{gaps:?}");
		let mean = gaps.iter().sum::<u64>() as f64 / gaps.len() as f64;
		assert!((mean - PING_MS as f64).abs() < 25.0, "mean {mean}");
		assert!(gaps.windows(2).all(|w| w[0] != w[1]), "uneven: {gaps:?}");
	}

	/// The RTT jumps from 78 ms to 600 ms and stays there: the readings follow it within a few
	/// Pongs instead of locking onto RTT − 500 (a fixed 500 ms cadence read a steady 100 ms).
	#[test]
	fn a_sustained_rtt_over_the_ping_cadence_is_read_right() {
		for jump in [600u64, 900, 1300] {
			let out = pong_run(40, |t| if t < 3000 { 78 } else { jump }, |_| false);
			let late: Vec<_> = out.iter().filter(|(t, _, _)| *t > 12_000).collect();
			assert!(late.len() > 40);
			let wrong = late.iter().filter(|(_, truth, r)| (*r - *truth as f32).abs() > 5.0).count();
			assert!(wrong == 0, "{jump} ms: {wrong} of {} wrong, e.g. {:?}", late.len(), &late[..6]);
		}
	}

	/// A short RTT with lost Pings: the Pong after a loss is timed against its own Ping, not
	/// +500 ms against the lost one; and a growing queue is followed.
	#[test]
	fn lost_pings_and_a_growing_queue_keep_the_readings_right() {
		let out = pong_run(40, |_| 78, |i| i % 3 == 1 || i % 7 == 0);
		// (The very first Pongs, before any sample, go to the oldest Ping as ever; Pings 0 and 1
		// are both lost here.)
		let wrong: Vec<_> =
			out.iter().filter(|(t, truth, r)| *t > 3000 && (*r - *truth as f32).abs() > 5.0).collect();
		assert!(wrong.is_empty(), "{} of {} wrong: {wrong:?}", wrong.len(), out.len());
		// 78 ms → 1 s over 20 s and back.
		let ramp = |t: u64| 78 + (t.saturating_sub(5000).min(20_000) * 922 / 20_000);
		let out = pong_run(40, ramp, |i| i % 5 == 2);
		let wrong = out.iter().filter(|(_, truth, r)| (*r - *truth as f32).abs() > 5.0).count();
		assert!(wrong * 10 <= out.len(), "{wrong} of {} wrong", out.len());
	}
}
