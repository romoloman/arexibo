// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Runtime state for Schedule Criteria (Xibo 4.1+) -- metrics pushed to
//! the player (via XMR `criteriaUpdate`, see xmr.rs) that schedule items
//! can be conditioned on (see schedule.rs's `ScheduleCriterion`).
//!
//! Deliberately in-memory only, not persisted across restarts: criteria
//! are meant to be pushed in real time by an external system (a
//! Data Connector or a direct API/XMR call), and are expected to be
//! refreshed periodically by whatever is setting them -- losing the
//! last-known value across a player restart is an acceptable, standard
//! trade-off (matches how a TTL-bounded value is supposed to behave
//! anyway: it's not meant to be trusted indefinitely without a
//! refresh).

use std::collections::HashMap;
use time::{Duration, OffsetDateTime};

#[derive(Debug, Default)]
pub struct CriteriaStore {
    values: HashMap<String, (String, OffsetDateTime)>,
}

impl CriteriaStore {
    /// Set (or replace) a metric's current value, expiring `ttl` seconds
    /// from now. A `ttl` of 0 or less is treated as "expires immediately"
    /// (matches the XMR message's own `ttl` semantics elsewhere in this
    /// codebase, e.g. xmr.rs's JsonMessage::is_expired).
    pub fn set(&mut self, metric: String, value: String, ttl: i64) {
        let expires = OffsetDateTime::now_utc() + Duration::seconds(ttl.max(0));
        self.values.insert(metric, (value, expires));
    }

    /// Remove any metrics whose ttl has elapsed -- called before every
    /// schedule evaluation (see mainloop.rs's schedule_check) so an
    /// expired criterion doesn't keep a schedule item falsely active (or
    /// falsely inactive, for a `ne`/not-equal condition).
    pub fn prune_expired(&mut self) {
        let now = OffsetDateTime::now_utc();
        self.values.retain(|_, (_, expires)| *expires > now);
    }

    /// Current value of a metric, if set and not (yet pruned as)
    /// expired.
    pub fn get(&self, metric: &str) -> Option<&str> {
        let now = OffsetDateTime::now_utc();
        self.values.get(metric)
            .filter(|(_, expires)| *expires > now)
            .map(|(v, _)| v.as_str())
    }
}

/// FLAGGED AS UNVERIFIED: the exact set of `condition` strings isn't
/// independently confirmed (only that a `condition` attribute exists).
/// Supports both short (eq/ne/gt/gte/lt/lte) and long (equals/
/// notEquals/greaterThan/...) forms defensively, matching the general
/// pattern in Xibo's module/widget rule system. Verify against a real
/// criteria-conditioned schedule item before relying on this for
/// anything safety-critical.
///
/// Numeric conditions require both `expected` and `actual` to parse as
/// f64 -- fail closed (not satisfied) if either doesn't. eq/ne fall
/// back to string comparison, since metrics like weather_condition are
/// legitimately non-numeric.
pub fn criterion_matches(condition: &str, expected: &str, actual: &str) -> bool {
    let nums = || -> Option<(f64, f64)> {
        Some((actual.parse().ok()?, expected.parse().ok()?))
    };
    match condition {
        "eq" | "equals" => nums().map(|(a, e)| a == e).unwrap_or(actual == expected),
        "ne" | "notEquals" => nums().map(|(a, e)| a != e).unwrap_or(actual != expected),
        "gt" | "greaterThan" => nums().is_some_and(|(a, e)| a > e),
        "gte" | "ge" | "greaterThanOrEqual" => nums().is_some_and(|(a, e)| a >= e),
        "lt" | "lessThan" => nums().is_some_and(|(a, e)| a < e),
        "lte" | "le" | "lessThanOrEqual" => nums().is_some_and(|(a, e)| a <= e),
        _ => {
            log::warn!("unknown schedule criteria condition {condition:?}, treating as not met");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_roundtrip() {
        let mut cs = CriteriaStore::default();
        assert_eq!(cs.get("temperature"), None);
        cs.set("temperature".into(), "22".into(), 3600);
        assert_eq!(cs.get("temperature"), Some("22"));
    }

    #[test]
    fn expired_values_are_not_returned() {
        let mut cs = CriteriaStore::default();
        cs.set("temperature".into(), "22".into(), -1); // already expired
        assert_eq!(cs.get("temperature"), None);
    }

    #[test]
    fn prune_expired_removes_stale_entries() {
        let mut cs = CriteriaStore::default();
        cs.set("a".into(), "1".into(), -1);
        cs.set("b".into(), "2".into(), 3600);
        cs.prune_expired();
        assert_eq!(cs.values.len(), 1);
        assert_eq!(cs.get("b"), Some("2"));
    }

    #[test]
    fn numeric_conditions() {
        assert!(criterion_matches("gt", "20", "25"));
        assert!(!criterion_matches("gt", "20", "15"));
        assert!(criterion_matches("gte", "20", "20"));
        assert!(criterion_matches("lt", "20", "15"));
        assert!(criterion_matches("lte", "20", "20"));
        assert!(criterion_matches("eq", "20", "20"));
        assert!(criterion_matches("ne", "20", "21"));
    }

    #[test]
    fn eq_falls_back_to_string_comparison_for_non_numeric() {
        assert!(criterion_matches("eq", "rain", "rain"));
        assert!(!criterion_matches("eq", "rain", "clear"));
        assert!(criterion_matches("ne", "rain", "clear"));
    }

    #[test]
    fn numeric_condition_with_non_numeric_value_fails_closed() {
        assert!(!criterion_matches("gt", "20", "rain"));
    }

    #[test]
    fn unknown_condition_fails_closed() {
        assert!(!criterion_matches("bogus", "1", "1"));
    }
}
