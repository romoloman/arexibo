// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Player fault reporting (XMDS `ReportFaults`, introduced in v6 -- see
//! account.xibosignage.com/docs/developer/creating-a-player/xmds, fetched
//! and read in full during development, not from memory).
//!
//! SCOPE: only the collection/submission infrastructure plus a single
//! concrete call site (layout translation/download failures, see
//! resource.rs) are wired up here. Xibo's own players also report faults
//! for e.g. individual video/media playback errors -- NOT implemented
//! here, deliberately scoped out rather than attempted half-done; the
//! `FaultCollector`/`record_fault` machinery is reusable for a follow-up
//! that adds more call sites.
//!
//! Confirmed JSON payload shape (one object per fault, submitted as a
//! JSON array):
//! ```json
//! {
//!   "code": 1000, "reason": "...", "date": "Y-m-d H:i:s",
//!   "expires": "Y-m-d H:i:s", "layoutId": 0, "regionId": 0,
//!   "widgetId": 0, "scheduleId": 0, "mediaId": 0
//! }
//! ```
//! Fault *codes* themselves are NOT independently confirmed here beyond
//! a couple of examples seen in passing on the community forum (e.g.
//! "2001" for a video codec issue) -- no canonical code list was found.
//! `FAULT_CODE_LAYOUT_TRANSLATE_FAILED` below is a locally-invented
//! placeholder, not a real Xibo fault code, clearly named as such.

use time::OffsetDateTime;
use serde::Serialize;
use crate::util::TIME_FMT;

/// Placeholder/local code for "a layout failed to download or translate"
/// -- FLAGGED AS UNVERIFIED, not a documented official Xibo fault code
/// (no canonical code list was found). Chosen to obviously stand out
/// (not overlapping likely-official low numbers like 1000/2001 seen in
/// passing) rather than to match a real registry.
pub const FAULT_CODE_LAYOUT_TRANSLATE_FAILED: i32 = 9001;

/// Reason strings longer than this get truncated before submission --
/// a real, confirmed bug (xibosignage/xibo#3230) causes the CMS to
/// reject the whole fault record if `reason` exceeds 255 characters
/// (a DB column limit), which is worse than a merely-truncated message.
const MAX_REASON_LEN: usize = 255;

#[derive(Debug, Clone, Serialize)]
pub struct Fault {
    pub code: i32,
    pub reason: String,
    #[serde(with = "fmt_time")]
    pub date: OffsetDateTime,
    #[serde(with = "fmt_time_opt")]
    pub expires: Option<OffsetDateTime>,
    #[serde(rename = "layoutId")]
    pub layout_id: i64,
    #[serde(rename = "regionId")]
    pub region_id: i64,
    #[serde(rename = "widgetId")]
    pub widget_id: i64,
    #[serde(rename = "scheduleId")]
    pub schedule_id: i64,
    #[serde(rename = "mediaId")]
    pub media_id: i64,
}

impl Fault {
    /// Build a fault with just a code/reason, all ids defaulted to 0
    /// (matching the "0 means not applicable" convention seen in the
    /// documented example payload) and no expiry.
    pub fn new(code: i32, reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        if reason.len() > MAX_REASON_LEN {
            // truncate at a char boundary, not a byte offset, to avoid
            // panicking on multi-byte UTF-8 (e.g. accented characters,
            // very plausible here given how much Italian text flows
            // through this whole codebase)
            let mut cut = MAX_REASON_LEN;
            while !reason.is_char_boundary(cut) { cut -= 1; }
            reason.truncate(cut);
        }
        Self {
            code, reason, date: OffsetDateTime::now_utc(), expires: None,
            layout_id: 0, region_id: 0, widget_id: 0, schedule_id: 0, media_id: 0,
        }
    }

    pub fn with_layout(mut self, layout_id: i64) -> Self {
        self.layout_id = layout_id;
        self
    }
}

mod fmt_time {
    use super::*;
    use serde::Serializer;
    pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&t.format(&TIME_FMT).unwrap_or_default())
    }
}

mod fmt_time_opt {
    use super::*;
    use serde::Serializer;
    pub fn serialize<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&t.format(&TIME_FMT).unwrap_or_default()),
            None => s.serialize_none(),
        }
    }
}

/// Defensive cap on unsent faults kept in memory, mirroring
/// stats.rs's StatCollector for the same reason (CMS unreachable for a
/// long time shouldn't grow this unboundedly).
const MAX_PENDING: usize = 200;

#[derive(Debug, Default)]
pub struct FaultCollector {
    pending: Vec<Fault>,
}

impl FaultCollector {
    pub fn record(&mut self, fault: Fault) {
        if self.pending.len() >= MAX_PENDING {
            log::warn!("fault collector: dropping oldest pending fault, \
                        CMS unreachable for too long? ({MAX_PENDING} pending)");
            self.pending.remove(0);
        }
        self.pending.push(fault);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Render all pending faults as the JSON array ReportFaults expects,
    /// and clear them -- caller must call `requeue` with the same
    /// faults if the actual submission fails.
    pub fn build_and_clear(&mut self) -> (String, Vec<Fault>) {
        let faults = std::mem::take(&mut self.pending);
        let json = serde_json::to_string(&faults).unwrap_or_else(|_| "[]".into());
        (json, faults)
    }

    pub fn requeue(&mut self, mut faults: Vec<Fault>) {
        faults.append(&mut self.pending);
        self.pending = faults;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_ids_to_zero_and_no_expiry() {
        let f = Fault::new(9001, "test");
        assert_eq!(f.code, 9001);
        assert_eq!(f.reason, "test");
        assert_eq!(f.layout_id, 0);
        assert_eq!(f.widget_id, 0);
        assert!(f.expires.is_none());
    }

    #[test]
    fn with_layout_sets_layout_id_only() {
        let f = Fault::new(1, "x").with_layout(612);
        assert_eq!(f.layout_id, 612);
        assert_eq!(f.region_id, 0);
    }

    #[test]
    fn long_reason_gets_truncated() {
        let long = "a".repeat(500);
        let f = Fault::new(1, long);
        assert_eq!(f.reason.len(), MAX_REASON_LEN);
    }

    #[test]
    fn truncation_respects_utf8_char_boundaries() {
        // multi-byte chars right around the truncation point must not
        // panic and must not produce invalid UTF-8
        let long = "à".repeat(300); // each char is 2 bytes in UTF-8
        let f = Fault::new(1, long);
        assert!(f.reason.len() <= MAX_REASON_LEN);
        assert!(f.reason.is_char_boundary(f.reason.len()));
    }

    #[test]
    fn build_and_clear_produces_valid_json_array() {
        let mut fc = FaultCollector::default();
        assert!(fc.is_empty());
        fc.record(Fault::new(9001, "layout 612 failed").with_layout(612));
        let (json, faults) = fc.build_and_clear();
        assert!(fc.is_empty());
        assert_eq!(faults.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["code"], 9001);
        assert_eq!(parsed[0]["layoutId"], 612);
        assert_eq!(parsed[0]["reason"], "layout 612 failed");
    }

    #[test]
    fn requeue_preserves_order_for_retry() {
        let mut fc = FaultCollector::default();
        fc.record(Fault::new(1, "first"));
        let (_, faults) = fc.build_and_clear();
        fc.record(Fault::new(2, "second"));
        fc.requeue(faults);
        let (_, all) = fc.build_and_clear();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].reason, "first");
        assert_eq!(all[1].reason, "second");
    }

    #[test]
    fn drops_oldest_when_over_capacity() {
        let mut fc = FaultCollector::default();
        for i in 0..MAX_PENDING + 5 {
            fc.record(Fault::new(i as i32, "x"));
        }
        let (_, faults) = fc.build_and_clear();
        assert_eq!(faults.len(), MAX_PENDING);
        assert_eq!(faults[0].code, 5);
    }
}
