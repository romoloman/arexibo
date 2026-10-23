// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Proof of Play statistics collection.
//!
//! Two kinds of record are produced: "layout" (a layout was shown) and
//! "media" (a widget was shown; the CMS looks the widget up by the id in
//! `mediaid` and corrects the type to "widget" for one without media).
//! "event" records are not implemented. Reference
//! (https://account.xibosignage.com/docs/developer/creating-a-player/
//! proof-of-play):
//! ```xml
//! <stats>
//!   <stat type="layout|media" fromdt="" todt="" layoutid="" scheduleid=""
//!         [mediaid=""] duration="" count="">
//!     <engagements><engagement tag="CRITERIA|a:b" duration="0" count="1"/>
//!     </engagements>
//!   </stat>
//! </stats>
//! ```
//! Aggregation (`Hourly`, `Daily`) adds the plays of a window up, as the
//! Windows player does, with one difference: its key lacks the layout, so
//! the layouts of one campaign (one `scheduleid`) were added together under
//! the first of them -- the bug reported for the Android player. The key
//! here is (type, schedule, layout, widget, window).
//!
//! Records not yet sent are kept in a file, so a restart does not lose them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, Time};
use crate::resource::LayoutId;
use crate::util::TIME_FMT;

/// The `aggregationLevel` of the CMS's display settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Aggregation {
    #[default]
    Individual,
    Hourly,
    Daily,
}

impl Aggregation {
    /// Anything but `hourly` or `daily` (any case) is `Individual`.
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "hourly" => Self::Hourly,
            "daily" => Self::Daily,
            _ => Self::Individual,
        }
    }

    /// The start of the window `at` is in; `None` if there are no windows.
    fn window_start(self, at: OffsetDateTime) -> Option<OffsetDateTime> {
        match self {
            Self::Individual => None,
            Self::Hourly => Some(at.replace_time(Time::from_hms(at.hour(), 0, 0).expect("valid"))),
            Self::Daily => Some(at.replace_time(Time::MIDNIGHT)),
        }
    }

    fn window(self, at: OffsetDateTime) -> Option<(OffsetDateTime, OffsetDateTime)> {
        let start = self.window_start(at)?;
        let length = if self == Self::Hourly { Duration::hours(1) } else { Duration::days(1) };
        Some((start, start + length))
    }
}

pub fn default_aggregation_level() -> String {
    "Individual".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatKind {
    #[default]
    Layout,
    Media,
}

impl StatKind {
    fn name(self) -> &'static str {
        match self {
            Self::Layout => "layout",
            Self::Media => "media",
        }
    }
}

/// One play, as it happened.
#[derive(Debug, Clone)]
pub struct Play {
    pub kind: StatKind,
    pub layoutid: LayoutId,
    pub scheduleid: i64,
    /// The widget of a `Media` play; 0 for a layout.
    pub widgetid: i64,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    /// The schedule criteria in force, as one engagement tag (see
    /// `criteria_tag`); empty if there were none.
    pub criteria: String,
}

/// What is kept until it is sent: one play, or the plays of a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatRecord {
    #[serde(default)]
    pub kind: StatKind,
    pub fromdt: OffsetDateTime,
    pub todt: OffsetDateTime,
    pub layoutid: LayoutId,
    pub scheduleid: i64,
    #[serde(default)]
    pub widgetid: i64,
    #[serde(default = "one")]
    pub count: u32,
    /// Seconds, the plays' rounded durations added up.
    #[serde(default)]
    pub duration: i64,
    /// Engagement tag -> number of plays it applied to.
    #[serde(default)]
    pub engagements: BTreeMap<String, u32>,
}

fn one() -> u32 {
    1
}

fn seconds(from: OffsetDateTime, to: OffsetDateTime) -> i64 {
    (to - from).as_seconds_f64().round().max(0.0) as i64
}

/// `CRITERIA|metric:value, metric:value`, as the Electron player writes it.
pub fn criteria_tag(active: &[(String, String)]) -> String {
    if active.is_empty() {
        return String::new();
    }
    let list: Vec<String> = active.iter().map(|(metric, value)| format!("{metric}:{value}")).collect();
    format!("CRITERIA|{}", list.join(", "))
}

fn xml_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

/// Cap on the unsent records kept, dropping the oldest, if the CMS is
/// unreachable for a long time.
const MAX_PENDING: usize = 500;

/// How often the file of unsent records is rewritten while records come in.
const SAVE_EVERY: StdDuration = StdDuration::from_secs(30);

#[derive(Serialize)]
struct StatFileRef<'a> {
    cms: &'a str,
    records: &'a [StatRecord],
}

#[derive(Deserialize)]
struct StatFile {
    cms: String,
    records: Vec<StatRecord>,
}

#[derive(Debug, Default)]
pub struct StatCollector {
    pending: Vec<StatRecord>,
    /// Where the unsent records are kept, and the CMS they are for.
    file: Option<(PathBuf, String)>,
    /// Records came in since the file was written.
    dirty: bool,
    last_save: Option<Instant>,
}

impl StatCollector {
    /// The collector, with the records an earlier run left unsent to this
    /// CMS (those of another CMS, or an unreadable file, are dropped).
    pub fn load(path: PathBuf, cms: &str) -> Self {
        let pending = match std::fs::read_to_string(&path) {
            Err(_) => Vec::new(),
            Ok(text) => match serde_json::from_str::<StatFile>(&text) {
                Ok(file) if file.cms == cms => {
                    let mut records = file.records;
                    let extra = records.len().saturating_sub(MAX_PENDING);
                    records.drain(..extra);
                    if !records.is_empty() {
                        log::info!("stats: {} unsent Proof of Play record(s) from the last run", records.len());
                    }
                    records
                }
                Ok(_) => {
                    log::info!("stats: the unsent records in {} are for another CMS, dropping them", path.display());
                    Vec::new()
                }
                Err(e) => {
                    log::warn!("stats: {} is unreadable, dropping it: {e}", path.display());
                    Vec::new()
                }
            },
        };
        Self { pending, file: Some((path, cms.to_string())), dirty: false, last_save: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|(path, _)| path.as_path())
    }

    /// Writes the records not yet sent (best effort: a failure is logged).
    pub fn save(&mut self) {
        self.dirty = false;
        self.last_save = Some(Instant::now());
        let Some((path, cms)) = &self.file else { return };
        let body = match serde_json::to_vec(&StatFileRef { cms, records: &self.pending }) {
            Ok(body) => body,
            Err(e) => return log::warn!("stats: serializing the unsent records: {e}"),
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, path)) {
            log::warn!("stats: writing {}: {e}", path.display());
        }
    }

    /// Writes the file if records came in since it was.
    pub fn flush_dirty(&mut self) {
        if self.dirty {
            self.save();
        }
    }

    fn changed(&mut self) {
        self.dirty = true;
        if self.last_save.map_or(true, |at| at.elapsed() >= SAVE_EVERY) {
            self.save();
        }
    }

    fn push(&mut self, record: StatRecord) {
        if self.pending.len() >= MAX_PENDING {
            log::warn!("stats: dropping oldest pending Proof of Play record, \
                        CMS unreachable for too long? ({MAX_PENDING} pending)");
            self.pending.remove(0);
        }
        self.pending.push(record);
    }

    /// Records a play: on its own, or added to the record of its window
    /// when the level aggregates.
    pub fn record(&mut self, play: Play, level: Aggregation) {
        let mut engagements = BTreeMap::new();
        if !play.criteria.is_empty() {
            engagements.insert(play.criteria.clone(), 1);
        }
        let mut record = StatRecord {
            kind: play.kind, fromdt: play.from, todt: play.to, layoutid: play.layoutid,
            scheduleid: play.scheduleid, widgetid: play.widgetid, count: 1,
            duration: seconds(play.from, play.to), engagements,
        };
        if let Some((start, end)) = level.window(play.from) {
            // A play that ends outside the window is not split: it stays
            // a record of its own, with its own times.
            if play.to <= end {
                record.fromdt = start;
                record.todt = end;
            }
            let same = |other: &StatRecord| {
                other.kind == record.kind && other.scheduleid == record.scheduleid
                    && other.layoutid == record.layoutid && other.widgetid == record.widgetid
                    && other.fromdt == record.fromdt && other.todt == record.todt
            };
            if let Some(existing) = self.pending.iter_mut().find(|other| same(other)) {
                existing.count += record.count;
                existing.duration += record.duration;
                for (tag, plays) in record.engagements {
                    *existing.engagements.entry(tag).or_default() += plays;
                }
                self.changed();
                return;
            }
        }
        self.push(record);
        self.changed();
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The records that are due at `now` as the `<stats>` XML for
    /// SubmitStats, taken out of the list: call `confirm_sent` once the CMS
    /// has them, or `requeue` them if the submission fails. The file keeps
    /// them until then. Due: all of them if there is no aggregation, else
    /// those of windows that are over -- the CMS is not sent a half hour.
    pub fn take_ready(&mut self, now: OffsetDateTime, level: Aggregation) -> (String, Vec<StatRecord>) {
        let cutoff = level.window_start(now);
        let (ready, kept): (Vec<StatRecord>, Vec<StatRecord>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|record| cutoff.map_or(true, |cutoff| record.todt <= cutoff));
        self.pending = kept;
        (xml_for(&ready), ready)
    }

    /// The CMS has what `take_ready` returned: forget it on disk too.
    pub fn confirm_sent(&mut self) {
        self.save();
    }

    /// Put records back at the front of the pending queue (so the next
    /// `take_ready` sends the oldest data first) after a failed submission.
    pub fn requeue(&mut self, mut records: Vec<StatRecord>) {
        records.append(&mut self.pending);
        let extra = records.len().saturating_sub(MAX_PENDING);
        records.drain(..extra);
        self.pending = records;
    }

    /// Everything pending, taken out (tests).
    #[cfg(test)]
    pub fn take_all(&mut self) -> Vec<StatRecord> {
        std::mem::take(&mut self.pending)
    }
}

fn xml_for(records: &[StatRecord]) -> String {
    let mut xml = String::from("<stats>");
    for rec in records {
        let widget = if rec.kind == StatKind::Media { format!(" mediaid=\"{}\"", rec.widgetid) } else { String::new() };
        xml.push_str(&format!(
            "<stat type=\"{}\" fromdt=\"{}\" todt=\"{}\" layoutid=\"{}\" scheduleid=\"{}\"{widget} \
             duration=\"{}\" count=\"{}\"",
            rec.kind.name(),
            rec.fromdt.format(&TIME_FMT).unwrap_or_default(),
            rec.todt.format(&TIME_FMT).unwrap_or_default(),
            rec.layoutid, rec.scheduleid, rec.duration, rec.count,
        ));
        if rec.engagements.is_empty() {
            xml.push_str(" />");
        } else {
            xml.push_str("><engagements>");
            for (tag, plays) in &rec.engagements {
                xml.push_str(&format!("<engagement tag=\"{}\" duration=\"0\" count=\"{plays}\" />", xml_attr(tag)));
            }
            xml.push_str("</engagements></stat>");
        }
    }
    xml.push_str("</stats>");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month};

    fn dt(hour: u8, min: u8, sec: u8) -> OffsetDateTime {
        OffsetDateTime::new_utc(
            Date::from_calendar_date(2026, Month::August, 5).unwrap(),
            Time::from_hms(hour, min, sec).unwrap(),
        )
    }

    fn day(d: u8, hour: u8, min: u8) -> OffsetDateTime {
        OffsetDateTime::new_utc(
            Date::from_calendar_date(2026, Month::August, d).unwrap(),
            Time::from_hms(hour, min, 0).unwrap(),
        )
    }

    fn layout_play(layoutid: i64, from: OffsetDateTime, to: OffsetDateTime) -> Play {
        Play { kind: StatKind::Layout, layoutid, scheduleid: 1, widgetid: 0, from, to, criteria: String::new() }
    }

    fn widget_play(layoutid: i64, widgetid: i64, from: OffsetDateTime, to: OffsetDateTime) -> Play {
        Play { kind: StatKind::Media, layoutid, scheduleid: 1, widgetid, from, to, criteria: String::new() }
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arexibo_stats_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("stats.json")
    }

    const IND: Aggregation = Aggregation::Individual;
    const HOUR: Aggregation = Aggregation::Hourly;
    const DAY: Aggregation = Aggregation::Daily;

    // ---- records and XML

    #[test]
    fn a_layout_play_becomes_the_expected_xml_and_leaves_the_list() {
        let mut sc = StatCollector::default();
        assert!(sc.is_empty());
        sc.record(Play { scheduleid: 109, ..layout_play(612, dt(10, 0, 0), dt(10, 5, 30)) }, IND);
        assert!(!sc.is_empty());
        let (xml, records) = sc.take_ready(dt(12, 0, 0), IND);
        assert!(sc.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(
            xml,
            "<stats><stat type=\"layout\" fromdt=\"2026-08-05 10:00:00\" \
             todt=\"2026-08-05 10:05:30\" layoutid=\"612\" scheduleid=\"109\" \
             duration=\"330\" count=\"1\" /></stats>"
        );
    }

    #[test]
    fn a_widget_play_is_a_media_record_with_the_widget_as_mediaid() {
        let mut sc = StatCollector::default();
        sc.record(Play { scheduleid: 109, ..widget_play(612, 4670, dt(10, 0, 0), dt(10, 0, 10)) }, IND);
        let (xml, _) = sc.take_ready(dt(12, 0, 0), IND);
        assert_eq!(
            xml,
            "<stats><stat type=\"media\" fromdt=\"2026-08-05 10:00:00\" \
             todt=\"2026-08-05 10:00:10\" layoutid=\"612\" scheduleid=\"109\" mediaid=\"4670\" \
             duration=\"10\" count=\"1\" /></stats>"
        );
    }

    #[test]
    fn the_duration_is_rounded_to_seconds_and_never_negative() {
        let mut sc = StatCollector::default();
        let t = dt(10, 0, 0);
        sc.record(layout_play(1, t, t + Duration::milliseconds(1600)), IND);
        sc.record(layout_play(2, t, t + Duration::milliseconds(1400)), IND);
        sc.record(layout_play(3, t + Duration::seconds(5), t), IND);
        let (xml, _) = sc.take_ready(dt(12, 0, 0), IND);
        assert!(xml.contains("layoutid=\"1\" scheduleid=\"1\" duration=\"2\""), "{xml}");
        assert!(xml.contains("layoutid=\"2\" scheduleid=\"1\" duration=\"1\""), "{xml}");
        assert!(xml.contains("layoutid=\"3\" scheduleid=\"1\" duration=\"0\""), "{xml}");
    }

    #[test]
    fn the_criteria_in_force_become_an_engagement() {
        let mut sc = StatCollector::default();
        sc.record(Play { criteria: "CRITERIA|weather:rain, meteo_temp:21".into(),
                         ..layout_play(1, dt(10, 0, 0), dt(10, 0, 10)) }, IND);
        sc.record(layout_play(2, dt(10, 0, 10), dt(10, 0, 20)), IND);
        let (xml, _) = sc.take_ready(dt(12, 0, 0), IND);
        assert!(xml.contains("count=\"1\"><engagements><engagement tag=\"CRITERIA|weather:rain, meteo_temp:21\" \
                              duration=\"0\" count=\"1\" /></engagements></stat>"), "{xml}");
        assert!(xml.contains("layoutid=\"2\" scheduleid=\"1\" duration=\"10\" count=\"1\" />"), "no criteria, none written: {xml}");
    }

    #[test]
    fn a_criterion_cannot_break_the_xml() {
        let mut sc = StatCollector::default();
        sc.record(Play { criteria: "CRITERIA|a:\"x\" & <y>]]>'".into(),
                         ..layout_play(1, dt(10, 0, 0), dt(10, 0, 10)) }, IND);
        let (xml, _) = sc.take_ready(dt(12, 0, 0), IND);
        assert!(xml.contains("tag=\"CRITERIA|a:&quot;x&quot; &amp; &lt;y&gt;]]&gt;&apos;\""), "{xml}");
        assert!(!xml.contains("]]>"), "{xml}");
    }

    #[test]
    fn the_criteria_tag_is_written_as_the_electron_player_does() {
        assert_eq!(criteria_tag(&[]), "");
        assert_eq!(criteria_tag(&[("weather".into(), "rain".into())]), "CRITERIA|weather:rain");
        assert_eq!(criteria_tag(&[("a".into(), "1".into()), ("b".into(), "2".into())]), "CRITERIA|a:1, b:2");
    }

    #[test]
    fn requeue_puts_records_back_in_order_for_retry() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(1, dt(10, 0, 0), dt(10, 5, 0)), IND);
        let (_, records) = sc.take_ready(dt(12, 0, 0), IND);
        sc.record(layout_play(2, dt(10, 5, 0), dt(10, 10, 0)), IND);
        sc.requeue(records);
        let (xml, records) = sc.take_ready(dt(12, 0, 0), IND);
        assert_eq!(records.iter().map(|r| r.layoutid).collect::<Vec<_>>(), [1, 2]);
        assert!(xml.find("layoutid=\"1\"").unwrap() < xml.find("layoutid=\"2\"").unwrap());
    }

    #[test]
    fn drops_oldest_when_over_capacity() {
        let mut sc = StatCollector::default();
        for i in 0..MAX_PENDING + 10 {
            sc.record(layout_play(i as i64, dt(10, 0, 0), dt(10, 0, 1)), IND);
        }
        let (_, records) = sc.take_ready(dt(12, 0, 0), IND);
        assert_eq!(records.len(), MAX_PENDING);
        assert_eq!(records[0].layoutid, 10, "the oldest 10 were dropped");
    }

    #[test]
    fn requeueing_respects_the_capacity_too() {
        let mut sc = StatCollector::default();
        for i in 0..MAX_PENDING {
            sc.record(layout_play(i as i64, dt(10, 0, 0), dt(10, 0, 1)), IND);
        }
        let (_, old) = sc.take_ready(dt(12, 0, 0), IND);
        sc.record(layout_play(9999, dt(10, 0, 0), dt(10, 0, 1)), IND);
        sc.requeue(old);
        assert_eq!(sc.pending.len(), MAX_PENDING);
        assert_eq!(sc.pending.last().unwrap().layoutid, 9999, "the newest is kept");
    }

    // ---- aggregation

    #[test]
    fn the_level_is_read_from_the_setting() {
        for (text, level) in [("Individual", IND), ("individual", IND), ("Hourly", HOUR), (" hourly ", HOUR),
                              ("DAILY", DAY), ("", IND), ("weekly", IND)] {
            assert_eq!(Aggregation::parse(text), level, "{text:?}");
        }
        assert_eq!(Aggregation::default(), IND);
    }

    #[test]
    fn hourly_adds_the_plays_of_an_hour_up() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(7, dt(10, 20, 0), dt(10, 20, 45)), HOUR);
        sc.record(layout_play(7, dt(10, 59, 0), dt(10, 59, 20)), HOUR);
        let (xml, records) = sc.take_ready(dt(11, 0, 0), HOUR);
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].count, records[0].duration), (3, 95));
        assert_eq!(xml, "<stats><stat type=\"layout\" fromdt=\"2026-08-05 10:00:00\" \
                         todt=\"2026-08-05 11:00:00\" layoutid=\"7\" scheduleid=\"1\" \
                         duration=\"95\" count=\"3\" /></stats>");
    }

    #[test]
    fn plays_of_different_hours_stay_apart() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(7, dt(11, 5, 0), dt(11, 5, 30)), HOUR);
        let (_, records) = sc.take_ready(dt(12, 0, 0), HOUR);
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn layouts_of_one_campaign_are_never_added_together() {
        // The Windows player's key has no layout in it: layouts sharing a
        // schedule were summed under the first (the Android bug).
        let mut sc = StatCollector::default();
        for layout in [7, 8, 7, 8, 8] {
            sc.record(layout_play(layout, dt(10, 5, 0), dt(10, 5, 10)), HOUR);
        }
        let (_, records) = sc.take_ready(dt(11, 0, 0), HOUR);
        assert_eq!(records.len(), 2);
        let by_layout: Vec<(i64, u32, i64)> = records.iter().map(|r| (r.layoutid, r.count, r.duration)).collect();
        assert_eq!(by_layout, [(7, 2, 20), (8, 3, 30)]);
    }

    #[test]
    fn schedules_widgets_and_kinds_are_not_added_together_either() {
        let mut sc = StatCollector::default();
        let (from, to) = (dt(10, 5, 0), dt(10, 5, 10));
        sc.record(layout_play(7, from, to), DAY);
        sc.record(Play { scheduleid: 2, ..layout_play(7, from, to) }, DAY);
        sc.record(widget_play(7, 100, from, to), DAY);
        sc.record(widget_play(7, 101, from, to), DAY);
        sc.record(widget_play(7, 100, from, to), DAY);
        let (_, records) = sc.take_ready(day(6, 0, 0), DAY);
        assert_eq!(records.len(), 4);
        let widget_100 = records.iter().find(|r| r.kind == StatKind::Media && r.widgetid == 100).unwrap();
        assert_eq!(widget_100.count, 2);
    }

    #[test]
    fn daily_adds_the_plays_of_a_day_up() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, day(5, 0, 0), day(5, 0, 0) + Duration::seconds(5)), DAY);
        sc.record(layout_play(7, day(5, 12, 0), day(5, 12, 0) + Duration::seconds(10)), DAY);
        sc.record(layout_play(7, day(5, 23, 59), day(5, 23, 59) + Duration::seconds(20)), DAY);
        let (xml, records) = sc.take_ready(day(6, 0, 0), DAY);
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].count, records[0].duration), (3, 35));
        assert!(xml.contains("fromdt=\"2026-08-05 00:00:00\" todt=\"2026-08-06 00:00:00\""), "{xml}");
    }

    #[test]
    fn a_play_that_ends_after_its_window_is_not_split_nor_added_to_others() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(7, dt(10, 59, 50), dt(11, 0, 20)), HOUR);
        let (xml, records) = sc.take_ready(dt(12, 0, 0), HOUR);
        assert_eq!(records.len(), 2);
        assert!(xml.contains("fromdt=\"2026-08-05 10:59:50\" todt=\"2026-08-05 11:00:20\""), "{xml}");
        assert!(xml.contains("fromdt=\"2026-08-05 10:00:00\" todt=\"2026-08-05 11:00:00\""), "{xml}");
    }

    #[test]
    fn only_windows_that_are_over_are_due() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(8, dt(11, 5, 0), dt(11, 5, 30)), HOUR);
        // 11:30: the 10:00 window is over, the 11:00 one is not
        let (_, due) = sc.take_ready(dt(11, 30, 0), HOUR);
        assert_eq!(due.iter().map(|r| r.layoutid).collect::<Vec<_>>(), [7]);
        assert_eq!(sc.pending.len(), 1, "the 11:00 window waits");
        // exactly at 12:00 it is over
        let (_, due) = sc.take_ready(dt(12, 0, 0), HOUR);
        assert_eq!(due.iter().map(|r| r.layoutid).collect::<Vec<_>>(), [8]);
        assert!(sc.is_empty());
    }

    #[test]
    fn the_current_day_waits_until_midnight() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, day(5, 10, 0), day(5, 10, 1)), DAY);
        let (_, due) = sc.take_ready(day(5, 23, 59), DAY);
        assert!(due.is_empty());
        let (_, due) = sc.take_ready(day(6, 0, 0), DAY);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn without_aggregation_everything_is_due_at_once() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), IND);
        sc.record(layout_play(7, dt(10, 6, 0), dt(10, 6, 30)), IND);
        let (_, due) = sc.take_ready(dt(10, 6, 31), IND);
        assert_eq!(due.len(), 2, "one record per play, not added up");
        assert!(due.iter().all(|r| r.count == 1));
    }

    #[test]
    fn switching_to_individual_sends_what_was_added_up_at_once() {
        let mut sc = StatCollector::default();
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(7, dt(10, 6, 0), dt(10, 6, 30)), HOUR);
        let (_, due) = sc.take_ready(dt(10, 7, 0), IND);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].count, 2);
    }

    #[test]
    fn engagements_are_added_per_tag() {
        let mut sc = StatCollector::default();
        for (criteria, minute) in [("CRITERIA|w:rain", 1), ("CRITERIA|w:sun", 2), ("CRITERIA|w:rain", 3), ("", 4)] {
            sc.record(Play { criteria: criteria.into(), ..layout_play(7, dt(10, minute, 0), dt(10, minute, 10)) }, HOUR);
        }
        let (xml, records) = sc.take_ready(dt(11, 0, 0), HOUR);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].count, 4);
        assert!(xml.contains("<engagement tag=\"CRITERIA|w:rain\" duration=\"0\" count=\"2\" />"), "{xml}");
        assert!(xml.contains("<engagement tag=\"CRITERIA|w:sun\" duration=\"0\" count=\"1\" />"), "{xml}");
    }

    // ---- the file

    #[test]
    fn unsent_records_survive_a_restart() {
        let path = temp_file("restart");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(Play { criteria: "CRITERIA|a:b".into(), ..layout_play(7, dt(10, 0, 0), dt(10, 0, 30)) }, IND);
        sc.record(layout_play(8, dt(10, 0, 30), dt(10, 1, 0)), IND);
        sc.save();
        let again = StatCollector::load(path, "http://cms");
        assert_eq!(again.pending.len(), 2);
        assert_eq!(again.pending[0].layoutid, 7);
        assert_eq!(again.pending[0].engagements.keys().next().unwrap(), "CRITERIA|a:b");
        assert_eq!(again.pending[1].layoutid, 8);
        assert_eq!(again.pending[0].fromdt, dt(10, 0, 0), "times survive exactly");
    }

    #[test]
    fn what_was_added_up_survives_a_restart_and_goes_on_being_added_to() {
        let path = temp_file("aggregated");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(widget_play(7, 100, dt(10, 5, 0), dt(10, 5, 10)), HOUR);
        sc.save();
        let mut again = StatCollector::load(path, "http://cms");
        again.record(layout_play(7, dt(10, 20, 0), dt(10, 20, 30)), HOUR);
        let (_, records) = again.take_ready(dt(11, 0, 0), HOUR);
        assert_eq!(records.len(), 2);
        let layout = records.iter().find(|r| r.kind == StatKind::Layout).unwrap();
        assert_eq!((layout.count, layout.duration), (2, 60));
    }

    #[test]
    fn records_come_in_faster_than_they_are_written() {
        let path = temp_file("throttle");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(layout_play(1, dt(10, 0, 0), dt(10, 0, 5)), IND);
        sc.record(layout_play(2, dt(10, 0, 5), dt(10, 0, 10)), IND);
        assert_eq!(StatCollector::load(path.clone(), "http://cms").pending.len(), 1, "the first is written at once");
        sc.flush_dirty();
        assert_eq!(StatCollector::load(path.clone(), "http://cms").pending.len(), 2);
        sc.flush_dirty();
        assert_eq!(StatCollector::load(path, "http://cms").pending.len(), 2, "nothing more to write");
    }

    #[test]
    fn records_for_another_cms_are_not_sent_to_this_one() {
        let path = temp_file("other_cms");
        let mut sc = StatCollector::load(path.clone(), "http://old-cms");
        sc.record(layout_play(7, dt(10, 0, 0), dt(10, 0, 30)), IND);
        assert!(StatCollector::load(path, "http://new-cms").is_empty());
    }

    #[test]
    fn an_unreadable_file_is_ignored() {
        for (name, content) in [("garbage", "not json"), ("empty", ""), ("wrong", r#"{"cms": 3}"#)] {
            let path = temp_file(name);
            std::fs::write(&path, content).unwrap();
            let mut sc = StatCollector::load(path.clone(), "http://cms");
            assert!(sc.is_empty(), "{name}");
            sc.record(layout_play(1, dt(10, 0, 0), dt(10, 0, 1)), IND);
            assert_eq!(StatCollector::load(path, "http://cms").pending.len(), 1, "{name}: and it is replaced");
        }
    }

    #[test]
    fn a_file_written_before_widgets_and_aggregation_still_loads() {
        let path = temp_file("old_format");
        let old = r#"{"cms":"http://cms","records":[{"fromdt":[2026,217,10,0,0,0,0,0,0],"todt":[2026,217,10,5,0,0,0,0,0],"layoutid":7,"scheduleid":3,"criteria":"CRITERIA|a:b"}]}"#;
        std::fs::write(&path, old).unwrap();
        let sc = StatCollector::load(path, "http://cms");
        assert_eq!(sc.pending.len(), 1);
        assert_eq!((sc.pending[0].kind, sc.pending[0].count, sc.pending[0].layoutid), (StatKind::Layout, 1, 7));
    }

    #[test]
    fn no_file_is_a_fresh_start() {
        assert!(StatCollector::load(temp_file("fresh"), "http://cms").is_empty());
    }

    #[test]
    fn a_confirmed_sending_empties_the_file_but_a_failed_one_keeps_it() {
        let path = temp_file("confirm");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(layout_play(1, dt(10, 0, 0), dt(10, 0, 5)), IND);
        let (_, records) = sc.take_ready(dt(12, 0, 0), IND);
        sc.requeue(records);
        assert_eq!(StatCollector::load(path.clone(), "http://cms").pending.len(), 1, "still on disk");
        let (_, _records) = sc.take_ready(dt(12, 0, 0), IND);
        sc.confirm_sent();
        assert!(StatCollector::load(path, "http://cms").is_empty());
    }

    #[test]
    fn a_partial_sending_leaves_the_rest_on_disk() {
        let path = temp_file("partial");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(layout_play(7, dt(10, 5, 0), dt(10, 5, 30)), HOUR);
        sc.record(layout_play(8, dt(11, 5, 0), dt(11, 5, 30)), HOUR);
        let (_, due) = sc.take_ready(dt(11, 30, 0), HOUR);
        assert_eq!(due.len(), 1);
        sc.confirm_sent();
        let left = StatCollector::load(path, "http://cms");
        assert_eq!(left.pending.len(), 1);
        assert_eq!(left.pending[0].layoutid, 8);
    }

    #[test]
    fn the_file_is_replaced_whole_and_leaves_no_temporary() {
        let path = temp_file("atomic");
        let mut sc = StatCollector::load(path.clone(), "http://cms");
        sc.record(layout_play(1, dt(10, 0, 0), dt(10, 0, 5)), IND);
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn more_than_the_cap_on_disk_keeps_the_newest() {
        let path = temp_file("cap");
        let records: Vec<StatRecord> = (0..MAX_PENDING + 20).map(|i| StatRecord {
            kind: StatKind::Layout, fromdt: dt(10, 0, 0), todt: dt(10, 0, 1), layoutid: i as i64, scheduleid: 1,
            widgetid: 0, count: 1, duration: 1, engagements: BTreeMap::new(),
        }).collect();
        std::fs::write(&path, serde_json::to_vec(&StatFileRef { cms: "http://cms", records: &records }).unwrap()).unwrap();
        let sc = StatCollector::load(path, "http://cms");
        assert_eq!(sc.pending.len(), MAX_PENDING);
        assert_eq!(sc.pending[0].layoutid, 20);
    }
}
