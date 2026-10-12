// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Sync Group Phase 5 (video wall drift correction) -- Lead-side
//! orchestration for one in-flight "preload, wait for everyone ready,
//! then play" round. Pure decision logic, no I/O and no real time
//! (the caller passes in `Instant::now()` and the live connected/ready
//! sets on every tick) -- this is what makes it fully unit-testable
//! without real threads, sleeps, or network connections; the actual
//! I/O (syncgroup::SyncGroup's own broadcast/reports channel,
//! server::VideoCommandStore) lives in mainloop.rs, which owns a
//! `RoundState` and drives it.
//!
//! A Follower that is *connected* but hasn't confirmed ready within
//! `confirm_timeout` yields `SkipToNext`; mainloop.rs plays anyway (the
//! Follower catches up through Position corrections) instead of
//! aborting the event. A Follower that has *disconnected* is given up on
//! after `disconnect_timeout` (`ProceedToPlay`). See AREXIBO_STATO_LAVORO.md.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// What the caller should do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Keep waiting -- nothing to do yet.
    Waiting,
    /// Every originally-connected Follower has either confirmed ready
    /// or been given up on as disconnected -- broadcast Play (and play
    /// locally, for the Lead's own copy).
    ProceedToPlay,
    /// A still-connected Follower failed to confirm in time (the caller
    /// decides what to do; mainloop.rs plays anyway).
    SkipToNext,
}

/// Tracks one round for one `sync_key`, from the moment Preload is
/// broadcast until the round resolves (Play or SkipToNext) -- a fresh
/// `Round` per content change, not reused across rounds.
pub struct Round {
    sync_key: String,
    started_at: Instant,
    confirm_timeout: Duration,
    disconnect_timeout: Duration,
    /// Followers connected when this round started -- a snapshot, not
    /// live-updating: a Follower connecting *after* this round began
    /// was not part of the original "everyone needs to confirm" set
    /// and is simply not waited for (it'll catch up via resync once
    /// the next content change comes around).
    originally_connected: HashSet<String>,
    ready: HashSet<String>,
    /// Followers from `originally_connected` no longer in the live
    /// connected set, and for how long -- tracked separately from
    /// `originally_connected` itself so a Follower that disconnects
    /// and reconnects within `disconnect_timeout` is simply treated as
    /// "still waiting on it", not specially reset.
    disconnected_since: std::collections::HashMap<String, Instant>,
}

impl Round {
    pub fn new(sync_key: String, originally_connected: Vec<String>, confirm_timeout: Duration,
               disconnect_timeout: Duration, now: Instant) -> Self {
        Self {
            sync_key, started_at: now, confirm_timeout, disconnect_timeout,
            originally_connected: originally_connected.into_iter().collect(),
            ready: HashSet::new(),
            disconnected_since: std::collections::HashMap::new(),
        }
    }

    pub fn sync_key(&self) -> &str {
        &self.sync_key
    }

    /// Records a "ready" report -- a no-op if `addr` wasn't part of
    /// this round's own original snapshot (e.g. a Follower that
    /// connected after this round started, or a stray/duplicate
    /// report), so it can never spuriously satisfy a round it was
    /// never being waited on for.
    pub fn record_ready(&mut self, addr: &str) {
        if self.originally_connected.contains(addr) {
            self.ready.insert(addr.to_string());
        }
    }

    /// `connected` is the *current* live set (see
    /// syncgroup::SyncGroup::connected_follower_addrs) -- called on
    /// every tick, not just once.
    pub fn tick(&mut self, connected: &[String], now: Instant) -> RoundOutcome {
        let connected: HashSet<&str> = connected.iter().map(String::as_str).collect();

        for addr in &self.originally_connected {
            let is_connected = connected.contains(addr.as_str());
            if is_connected {
                self.disconnected_since.remove(addr);
            } else {
                self.disconnected_since.entry(addr.clone()).or_insert(now);
            }
        }

        for addr in &self.originally_connected {
            if self.ready.contains(addr) {
                continue; // this one's fine
            }
            match self.disconnected_since.get(addr) {
                // Still connected, still not confirmed -- past its
                // patience?
                None => {
                    if now.duration_since(self.started_at) >= self.confirm_timeout {
                        return RoundOutcome::SkipToNext;
                    }
                    return RoundOutcome::Waiting;
                }
                // Disconnected -- past the (separate, typically
                // longer) patience for giving up on it entirely?
                Some(&since) => {
                    if now.duration_since(since) < self.disconnect_timeout {
                        return RoundOutcome::Waiting;
                    }
                    // else: given up on this one, keep checking the rest
                }
            }
        }
        RoundOutcome::ProceedToPlay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIRM: Duration = Duration::from_secs(5);
    const DISCONNECT: Duration = Duration::from_secs(20);

    #[test]
    fn waits_when_nobody_has_confirmed_yet_and_still_connected() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        assert_eq!(round.tick(&["10.0.0.1".into()], now), RoundOutcome::Waiting);
    }

    #[test]
    fn proceeds_once_every_originally_connected_follower_confirms() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(),
            vec!["10.0.0.1".into(), "10.0.0.2".into()], CONFIRM, DISCONNECT, now);
        round.record_ready("10.0.0.1");
        assert_eq!(round.tick(&["10.0.0.1".into(), "10.0.0.2".into()], now), RoundOutcome::Waiting,
                   "10.0.0.2 has not confirmed yet");
        round.record_ready("10.0.0.2");
        assert_eq!(round.tick(&["10.0.0.1".into(), "10.0.0.2".into()], now),
                   RoundOutcome::ProceedToPlay);
    }

    #[test]
    fn skips_to_next_when_a_still_connected_follower_never_confirms_in_time() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        let later = now + CONFIRM + Duration::from_millis(1);
        assert_eq!(round.tick(&["10.0.0.1".into()], later), RoundOutcome::SkipToNext);
    }

    #[test]
    fn does_not_skip_before_the_confirm_timeout_elapses() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        let almost = now + CONFIRM - Duration::from_millis(1);
        assert_eq!(round.tick(&["10.0.0.1".into()], almost), RoundOutcome::Waiting);
    }

    #[test]
    fn a_disconnected_follower_does_not_trigger_skip_to_next() {
        // The whole point of distinguishing disconnected from
        // connected-but-silent: a genuinely gone Follower must not
        // abort the round for the ones that ARE still here.
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        let later = now + CONFIRM + Duration::from_secs(1); // past confirm_timeout
        // Not in `connected` at all -- disconnected, not just silent.
        assert_eq!(round.tick(&[], later), RoundOutcome::Waiting,
                   "must wait out disconnect_timeout, not skip immediately");
    }

    #[test]
    fn proceeds_without_a_follower_once_its_disconnect_timeout_elapses() {
        // Realistic usage: mainloop.rs ticks this periodically, so the
        // disconnection gets marked shortly after it actually happens
        // -- not discovered for the first time long after the fact
        // (see this same test's own second tick, which relies on that).
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        assert_eq!(round.tick(&[], now), RoundOutcome::Waiting, "just noticed the disconnect");
        let later = now + DISCONNECT + Duration::from_millis(1);
        assert_eq!(round.tick(&[], later), RoundOutcome::ProceedToPlay);
    }

    #[test]
    fn a_follower_reconnecting_within_disconnect_timeout_is_just_waited_on_again() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        let mid = now + Duration::from_secs(10); // disconnected, but < DISCONNECT
        assert_eq!(round.tick(&[], mid), RoundOutcome::Waiting);
        // Reconnects, still hasn't confirmed -- back to being a
        // "connected but silent" case, its own confirm_timeout no
        // longer meaningfully applies since it restarts tracking from
        // when it reconnected, not from the round's own start... but
        // the *round's* start-based confirm_timeout has ALREADY
        // elapsed by `mid` (10s > CONFIRM's 5s) -- since it's back in
        // `connected` with `disconnected_since` cleared, the very next
        // tick re-evaluates it as "connected, over confirm_timeout" and
        // skips. This is intentional: a Follower rejoining late into an
        // already-overdue round doesn't get a fresh grace period.
        assert_eq!(round.tick(&["10.0.0.1".into()], mid), RoundOutcome::SkipToNext);
    }

    #[test]
    fn a_ready_report_from_a_follower_outside_the_original_snapshot_is_ignored() {
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec!["10.0.0.1".into()], CONFIRM, DISCONNECT, now);
        round.record_ready("10.0.0.99"); // never part of this round
        assert_eq!(round.tick(&["10.0.0.1".into()], now), RoundOutcome::Waiting,
                   "an unrelated report must not satisfy 10.0.0.1's own pending confirmation");
    }

    #[test]
    fn a_round_with_no_followers_at_all_proceeds_immediately() {
        // Sync Group with only a Lead and no Followers connected yet --
        // there's nobody to wait for.
        let now = Instant::now();
        let mut round = Round::new("key1".into(), vec![], CONFIRM, DISCONNECT, now);
        assert_eq!(round.tick(&[], now), RoundOutcome::ProceedToPlay);
    }
}
