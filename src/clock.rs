//! Compares this player's clock with the CMS's, taken from the HTTP `Date`
//! header of its answers: XMR messages and time-based schedules depend on
//! the two agreeing.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use once_cell::sync::Lazy;
use time::{format_description::well_known::Rfc2822, OffsetDateTime};

/// Beyond this the clocks are taken to disagree (HTTP dates have a
/// one-second resolution, and the answer takes a moment to arrive).
pub const SKEW_LIMIT_SECS: i64 = 60;
const REPEAT_WARNING_EVERY: Duration = Duration::from_secs(3600);

/// "1h 59m 59s", "3m 02s", "45s".
pub fn human_duration(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "" };
    let s = secs.unsigned_abs();
    let (h, m, sec) = (s / 3600, s % 3600 / 60, s % 60);
    let body = if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    };
    format!("{sign}{body}")
}

/// An HTTP date such as `Tue, 20 Oct 2026 08:31:44 GMT`.
pub fn parse_http_date(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    let rfc2822 = match value.strip_suffix("GMT") {
        Some(rest) => format!("{rest}+0000"),
        None => value.to_string(),
    };
    OffsetDateTime::parse(&rfc2822, &Rfc2822).ok()
}

/// Seconds this player's clock is ahead of the CMS's (negative: behind).
pub fn skew_secs(local: OffsetDateTime, server: OffsetDateTime) -> i64 {
    (local - server).whole_seconds()
}

pub fn disagreement_text(skew_secs: i64, local: OffsetDateTime, server: OffsetDateTime) -> String {
    let direction = if skew_secs > 0 { "ahead of" } else { "behind" };
    format!("the clock of this player is {} {direction} the CMS's (here {local}, CMS {server}) -- \
             XMR messages (screenshot, collect now, ...) older than their ttl of a couple of minutes \
             are dropped, and what is scheduled by time starts at the wrong time; check NTP \
             (timedatectl, chronyc tracking) and the time zones on both sides",
            human_duration(skew_secs.abs()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClockEvent {
    Nothing,
    Disagree,
    Agree,
}

/// Remembers whether the clocks disagree, so that a lasting difference is
/// reported once and then now and then, and the end of it once.
#[derive(Default)]
pub struct ClockWatch {
    disagreeing: bool,
    last_warning: Option<Instant>,
}

impl ClockWatch {
    pub fn observe(&mut self, skew_secs: i64, now: Instant) -> ClockEvent {
        if skew_secs.abs() <= SKEW_LIMIT_SECS {
            self.last_warning = None;
            return if std::mem::take(&mut self.disagreeing) { ClockEvent::Agree } else { ClockEvent::Nothing };
        }
        self.disagreeing = true;
        match self.last_warning {
            Some(at) if now.duration_since(at) < REPEAT_WARNING_EVERY => ClockEvent::Nothing,
            _ => {
                self.last_warning = Some(now);
                ClockEvent::Disagree
            }
        }
    }
}

static WATCH: Lazy<Mutex<ClockWatch>> = Lazy::new(|| Mutex::new(ClockWatch::default()));

/// Called with the `Date` header of an answer from the CMS.
pub fn check_against_cms(date_header: Option<&str>) {
    let Some(server) = date_header.and_then(parse_http_date) else { return };
    let local = OffsetDateTime::now_utc();
    let skew = skew_secs(local, server);
    match WATCH.lock().unwrap_or_else(|e| e.into_inner()).observe(skew, Instant::now()) {
        ClockEvent::Disagree => log::warn!("{}", disagreement_text(skew, local, server)),
        ClockEvent::Agree => log::info!("the clock of this player agrees with the CMS's again"),
        ClockEvent::Nothing => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[test]
    fn durations_are_written_for_people() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(182), "3m 02s");
        assert_eq!(human_duration(7199), "1h 59m 59s");
        assert_eq!(human_duration(7200), "2h 00m 00s");
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(-61), "-1m 01s");
    }

    #[test]
    fn an_http_date_is_read_as_utc() {
        let expected = at("2026-10-20T08:31:44Z");
        assert_eq!(parse_http_date("Tue, 20 Oct 2026 08:31:44 GMT"), Some(expected));
        assert_eq!(parse_http_date("  Tue, 20 Oct 2026 08:31:44 GMT \n"), Some(expected));
        assert_eq!(parse_http_date("Tue, 20 Oct 2026 10:31:44 +0200"), Some(expected));
    }

    #[test]
    fn something_that_is_not_a_date_is_nothing() {
        for text in ["", "yesterday", "2026-10-20", "Tue, 32 Oct 2026 08:31:44 GMT"] {
            assert_eq!(parse_http_date(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_skew_has_the_sign_of_where_this_player_is() {
        let server = at("2026-10-20T08:31:44Z");
        assert_eq!(skew_secs(at("2026-10-20T10:31:44Z"), server), 7200);
        assert_eq!(skew_secs(at("2026-10-20T08:30:44Z"), server), -60);
        assert_eq!(skew_secs(server, server), 0);
    }

    #[test]
    fn the_warning_says_which_way_and_by_how_much() {
        let local = at("2026-10-20T10:31:44Z");
        let server = at("2026-10-20T08:31:44Z");
        let ahead = disagreement_text(7200, local, server);
        assert!(ahead.contains("2h 00m 00s ahead of the CMS"), "{ahead}");
        let behind = disagreement_text(-90, server, local);
        assert!(behind.contains("1m 30s behind the CMS"), "{behind}");
        assert!(ahead.contains("XMR") && ahead.contains("chronyc") && ahead.contains("time zones"));
    }

    #[test]
    fn a_difference_within_the_limit_is_not_one() {
        let mut watch = ClockWatch::default();
        let now = Instant::now();
        for skew in [0, 1, -1, 60, -60] {
            assert_eq!(watch.observe(skew, now), ClockEvent::Nothing, "{skew}");
        }
        assert_eq!(watch.observe(61, now), ClockEvent::Disagree);
        let mut watch = ClockWatch::default();
        assert_eq!(watch.observe(-61, now), ClockEvent::Disagree, "behind counts too");
    }

    #[test]
    fn a_lasting_difference_is_reported_now_and_then() {
        let mut watch = ClockWatch::default();
        let t0 = Instant::now();
        assert_eq!(watch.observe(7200, t0), ClockEvent::Disagree);
        assert_eq!(watch.observe(7200, t0 + Duration::from_secs(600)), ClockEvent::Nothing);
        assert_eq!(watch.observe(7190, t0 + Duration::from_secs(3599)), ClockEvent::Nothing);
        assert_eq!(watch.observe(7200, t0 + Duration::from_secs(3600)), ClockEvent::Disagree, "again after an hour");
    }

    #[test]
    fn the_end_of_a_difference_is_reported_once() {
        let mut watch = ClockWatch::default();
        let t0 = Instant::now();
        assert_eq!(watch.observe(7200, t0), ClockEvent::Disagree);
        assert_eq!(watch.observe(1, t0 + Duration::from_secs(300)), ClockEvent::Agree);
        assert_eq!(watch.observe(0, t0 + Duration::from_secs(600)), ClockEvent::Nothing);
        // and a new difference is a new report, whatever the time since the last
        assert_eq!(watch.observe(7200, t0 + Duration::from_secs(700)), ClockEvent::Disagree);
    }
}
