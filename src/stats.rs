// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Proof of Play statistics collection.
//!
//! SCOPE: only "layout" records are produced (a layout started/stopped
//! showing). The documented format also supports "media"/"widget" records
//! (individual widget play time within a layout) and "event" records
//! (arbitrary tagged engagement data) -- neither is implemented here.
//! Widget-level records in particular would need each widget's own
//! start/stop signalled from JS up to Rust (region_switch in layout.rs's
//! SCRIPT would need to call a new jsWidgetShown/jsWidgetHidden bridge
//! method, mirroring how jsLayoutInit/jsLayoutDone already work) --
//! meaningfully more invasive than layout-level tracking, which only
//! needs the FromGui::Showing signal that already exists. Left as a
//! deliberate, scoped-out follow-up rather than attempted half-done here.
//!
//! Format reference (https://account.xibosignage.com/docs/developer/
//! creating-a-player/proof-of-play, fetched and confirmed during
//! development -- not from memory):
//! ```xml
//! <stats>
//!     <stat type="layout|media/widget|event" fromdt="" todt=""
//!           layoutid="" scheduleid="" mediaId="" tag="" />
//! </stats>
//! ```

use time::OffsetDateTime;
use crate::resource::LayoutId;
use crate::util::TIME_FMT;

/// A single accumulated "layout" Proof of Play record -- see module docs
/// for why only this record type exists so far.
#[derive(Debug, Clone)]
pub struct LayoutStat {
    pub fromdt: OffsetDateTime,
    pub todt: OffsetDateTime,
    pub layoutid: LayoutId,
    pub scheduleid: i64,
}

/// Defensive cap on how many unsent records are kept in memory if the CMS
/// is unreachable for a long time -- drops the *oldest* record and logs a
/// warning rather than growing unboundedly. 500 is arbitrary but generous
/// (at a bare minimum of ~10s per layout play, that's over an hour of
/// continuous layout changes before anything would be dropped).
const MAX_PENDING: usize = 500;

#[derive(Debug, Default)]
pub struct StatCollector {
    pending: Vec<LayoutStat>,
}

impl StatCollector {
    pub fn record_layout(&mut self, rec: LayoutStat) {
        if self.pending.len() >= MAX_PENDING {
            log::warn!("stats: dropping oldest pending Proof of Play record, \
                        CMS unreachable for too long? ({MAX_PENDING} pending)");
            self.pending.remove(0);
        }
        self.pending.push(rec);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Render all currently-pending records as the `<stats>...</stats>`
    /// XML expected by XMDS's SubmitStats, and remove them from the
    /// pending list -- the caller is responsible for calling [`requeue`]
    /// with the same records if the actual XMDS submission fails, so
    /// they aren't silently lost on a transient network error.
    ///
    /// [`requeue`]: StatCollector::requeue
    pub fn build_and_clear(&mut self) -> (String, Vec<LayoutStat>) {
        let recs = std::mem::take(&mut self.pending);
        let mut xml = String::from("<stats>");
        for rec in &recs {
            // No escaping needed: every value here is either a plain
            // integer (layoutid/scheduleid) or a value produced by
            // `TIME_FMT`'s own fixed "[year]-[month]-[day] ..." format,
            // neither of which can contain XML special characters.
            xml.push_str(&format!(
                "<stat type=\"layout\" fromdt=\"{}\" todt=\"{}\" \
                 layoutid=\"{}\" scheduleid=\"{}\" />",
                rec.fromdt.format(&TIME_FMT).unwrap_or_default(),
                rec.todt.format(&TIME_FMT).unwrap_or_default(),
                rec.layoutid, rec.scheduleid,
            ));
        }
        xml.push_str("</stats>");
        (xml, recs)
    }

    /// Put records back at the front of the pending queue (so the next
    /// [`build_and_clear`] sends the oldest data first) after a failed
    /// submission attempt.
    ///
    /// [`build_and_clear`]: StatCollector::build_and_clear
    pub fn requeue(&mut self, mut recs: Vec<LayoutStat>) {
        recs.append(&mut self.pending);
        self.pending = recs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Time, Month};

    fn dt(hour: u8, min: u8, sec: u8) -> OffsetDateTime {
        OffsetDateTime::new_utc(
            Date::from_calendar_date(2026, Month::August, 5).unwrap(),
            Time::from_hms(hour, min, sec).unwrap(),
        )
    }

    #[test]
    fn build_and_clear_produces_expected_xml_and_empties_pending() {
        let mut sc = StatCollector::default();
        assert!(sc.is_empty());
        sc.record_layout(LayoutStat {
            fromdt: dt(10, 0, 0),
            todt: dt(10, 5, 30),
            layoutid: 612,
            scheduleid: 109,
        });
        assert!(!sc.is_empty());
        let (xml, recs) = sc.build_and_clear();
        assert!(sc.is_empty());
        assert_eq!(recs.len(), 1);
        assert_eq!(
            xml,
            "<stats><stat type=\"layout\" fromdt=\"2026-08-05 10:00:00\" \
             todt=\"2026-08-05 10:05:30\" layoutid=\"612\" scheduleid=\"109\" /></stats>"
        );
    }

    #[test]
    fn requeue_puts_records_back_in_order_for_retry() {
        let mut sc = StatCollector::default();
        sc.record_layout(LayoutStat {
            fromdt: dt(10, 0, 0),
            todt: dt(10, 5, 0),
            layoutid: 1, scheduleid: 1,
        });
        let (_, recs) = sc.build_and_clear();
        // simulate a new record arriving while the (failed) submission
        // was in flight
        sc.record_layout(LayoutStat {
            fromdt: dt(10, 5, 0),
            todt: dt(10, 10, 0),
            layoutid: 2, scheduleid: 1,
        });
        sc.requeue(recs);
        let (xml, recs) = sc.build_and_clear();
        assert_eq!(recs.len(), 2);
        // the requeued (older) record must come first
        assert_eq!(recs[0].layoutid, 1);
        assert_eq!(recs[1].layoutid, 2);
        assert!(xml.find("layoutid=\"1\"").unwrap() < xml.find("layoutid=\"2\"").unwrap());
    }

    #[test]
    fn drops_oldest_when_over_capacity() {
        let mut sc = StatCollector::default();
        for i in 0..MAX_PENDING + 10 {
            sc.record_layout(LayoutStat {
                fromdt: dt(10, 0, 0),
                todt: dt(10, 0, 1),
                layoutid: i as i64, scheduleid: 1,
            });
        }
        let (_, recs) = sc.build_and_clear();
        assert_eq!(recs.len(), MAX_PENDING);
        // the oldest 10 (layoutid 0..10) should have been dropped
        assert_eq!(recs[0].layoutid, 10);
    }
}
