//! Data Connectors (real-time data), modelled on the official Electron
//! player (`electron-player`, `src/renderer/src/dataConnector/`). This file
//! holds the player-side rules: which scripts run, the host lifecycle and the
//! write guardrails. Running the scripts is done elsewhere.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::schedule::DataConnectorEntry;
use crate::util::percent_decode;

/// A host is stopped after this many consecutive assessments without its connector.
pub const STOP_AFTER_MISSED_TICKS: u32 = 3;
/// Minimum time between two stored writes of the same key (latest value wins).
pub const MIN_WRITE_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum value size, in UTF-16 code units (as in the Electron player).
pub const MAX_DATA_LENGTH: usize = 2 * 1024 * 1024;
pub const MAX_KEYS_PER_DATASET: usize = 100;

/// A connector ready to run: eligible now, with its script downloaded and verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorPayload {
    pub data_set_id: i64,
    pub schedule_id: i64,
    /// As sent by the CMS (URL-encoded).
    pub data_params: String,
    pub js: String,
    pub md5: String,
}

impl ConnectorPayload {
    /// A running host is restarted when this changes.
    pub fn state_key(&self) -> String {
        format!("{}|{}|{}", self.schedule_id, self.data_params, self.md5)
    }
}

/// Builds the payloads of the given connectors, skipping those whose script
/// is not available and verified (`script_md5` returns its md5, or `None`).
pub fn payloads(entries: &[&DataConnectorEntry],
                script_md5: impl Fn(&str) -> Option<String>) -> Vec<ConnectorPayload> {
    entries.iter().filter_map(|e| match script_md5(&e.js) {
        Some(md5) => Some(ConnectorPayload {
            data_set_id: e.data_set_id,
            schedule_id: e.schedule_id,
            data_params: e.data_params.clone(),
            js: e.js.clone(),
            md5,
        }),
        None => {
            log::debug!("data connector {} script {} not ready, skipping", e.data_set_id, e.js);
            None
        }
    }).collect()
}

/// The CMS urlencode()s the parameters (`+` for a space): decode them before
/// handing them to the script.
pub fn decode_data_params(raw: &str) -> String {
    percent_decode(&raw.replace('+', " "))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCommand {
    Start(ConnectorPayload),
    Stop(i64),
}

struct Host {
    state_key: String,
    missed_ticks: u32,
}

/// The connectors that currently have a running host.
#[derive(Default)]
pub struct Hosts {
    hosts: HashMap<i64, Host>,
}

impl Hosts {
    /// Compares the wanted connectors with the running hosts and returns what
    /// to stop (first) and start. A changed connector is stopped and started
    /// again; an unwanted one only after `STOP_AFTER_MISSED_TICKS` misses.
    pub fn sync(&mut self, desired: Vec<ConnectorPayload>) -> Vec<HostCommand> {
        let mut want: HashMap<i64, ConnectorPayload> =
            desired.into_iter().map(|p| (p.data_set_id, p)).collect();
        let mut commands = Vec::new();

        let mut running: Vec<i64> = self.hosts.keys().copied().collect();
        running.sort_unstable();
        for id in running {
            let host = self.hosts.get_mut(&id).expect("running host");
            match want.get(&id) {
                None => {
                    host.missed_ticks += 1;
                    if host.missed_ticks >= STOP_AFTER_MISSED_TICKS {
                        self.hosts.remove(&id);
                        commands.push(HostCommand::Stop(id));
                    }
                }
                Some(p) => {
                    host.missed_ticks = 0;
                    if host.state_key == p.state_key() {
                        want.remove(&id);
                    } else {
                        self.hosts.remove(&id);
                        commands.push(HostCommand::Stop(id));
                    }
                }
            }
        }

        let mut starts: Vec<ConnectorPayload> = want.into_values().collect();
        starts.sort_by_key(|p| p.data_set_id);
        for p in starts {
            self.hosts.insert(p.data_set_id, Host { state_key: p.state_key(), missed_ticks: 0 });
            commands.push(HostCommand::Start(p));
        }
        commands
    }

    /// A start that could not be carried out: forget the host so the next
    /// `sync` tries again.
    pub fn forget(&mut self, data_set_id: i64) {
        self.hosts.remove(&data_set_id);
    }

    pub fn active_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.hosts.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub data_set_id: i64,
    pub key: String,
    pub data: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// Store it now.
    Write(Write),
    /// Kept as the latest value; `due` returns it once the interval has passed.
    Queued,
    Rejected { status: u16, message: &'static str },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Flush {
    pub write: Write,
    /// A notification for this key was waiting for the write.
    pub notify: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotifyAction {
    Now,
    /// A write is pending: `due` reports the notification with it.
    WhenWritten,
}

#[derive(Default)]
struct KeyState {
    last_write: Option<Instant>,
    queued: Option<Write>,
    notify_pending: bool,
}

/// Rules for the values a connector publishes (`xiboDC.setData`): sizes, key
/// ownership and limits, and at most one stored write per key every
/// `MIN_WRITE_INTERVAL`.
#[derive(Default)]
pub struct RealtimeWriter {
    keys: HashMap<String, KeyState>,
    owners: HashMap<String, i64>,
    warned: HashSet<String>,
}

impl RealtimeWriter {
    fn warn_once(&mut self, id: String, message: String) {
        if self.warned.insert(id) {
            log::warn!("{message}");
        }
    }

    fn keys_owned_by(&self, data_set_id: i64) -> usize {
        self.owners.values().filter(|&&o| o == data_set_id).count()
    }

    pub fn set(&mut self, now: Instant, data_set_id: i64, key: &str, data: String) -> SetOutcome {
        if key.is_empty() {
            return SetOutcome::Rejected { status: 400, message: "A dataKey is required" };
        }
        if data.encode_utf16().count() > MAX_DATA_LENGTH {
            self.warn_once(format!("{key}:size"), format!(
                "data connector {data_set_id} exceeded the maximum data size for key {key}"));
            return SetOutcome::Rejected { status: 413, message: "Data too large" };
        }

        // Another connector writing the same key: the last write wins.
        let owner = self.owners.get(key).copied();
        if let Some(other) = owner.filter(|&o| o != data_set_id) {
            self.warn_once(format!("{key}:owner"), format!(
                "data key {key} is written by several data connectors ({other}, {data_set_id})"));
        }
        self.owners.insert(key.to_string(), data_set_id);

        if owner.is_none() && self.keys_owned_by(data_set_id) > MAX_KEYS_PER_DATASET {
            self.owners.remove(key);
            self.warn_once(format!("{data_set_id}:keys"), format!(
                "data connector {data_set_id} exceeded the maximum number of data keys"));
            return SetOutcome::Rejected { status: 429, message: "Too many data keys" };
        }

        let state = self.keys.entry(key.to_string()).or_default();
        let write = Write { data_set_id, key: key.to_string(), data };
        let interval_passed = state.last_write
            .map_or(true, |t| now.duration_since(t) >= MIN_WRITE_INTERVAL);
        if state.queued.is_none() && interval_passed {
            state.last_write = Some(now);
            SetOutcome::Write(write)
        } else {
            state.queued = Some(write);
            SetOutcome::Queued
        }
    }

    /// Queued writes whose interval has passed, by key.
    pub fn due(&mut self, now: Instant) -> Vec<Flush> {
        let mut keys: Vec<&String> = self.keys.iter()
            .filter(|(_, s)| s.queued.is_some()
                && s.last_write.map_or(true, |t| now.duration_since(t) >= MIN_WRITE_INTERVAL))
            .map(|(k, _)| k).collect();
        keys.sort();
        let keys: Vec<String> = keys.into_iter().cloned().collect();
        keys.into_iter().filter_map(|key| {
            let state = self.keys.get_mut(&key)?;
            let write = state.queued.take()?;
            state.last_write = Some(now);
            Some(Flush { write, notify: std::mem::take(&mut state.notify_pending) })
        }).collect()
    }

    /// When the next queued write becomes due.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.keys.values()
            .filter(|s| s.queued.is_some())
            .filter_map(|s| s.last_write.map(|t| t + MIN_WRITE_INTERVAL))
            .min()
    }

    /// `xiboDC.notifyHost(key)`: notify now, unless a write for the key is
    /// pending (widgets must not read a stale value after being told).
    pub fn notify(&mut self, key: &str) -> NotifyAction {
        match self.keys.get_mut(key) {
            Some(state) if state.queued.is_some() => {
                state.notify_pending = true;
                NotifyAction::WhenWritten
            }
            _ => NotifyAction::Now,
        }
    }

    /// A connector stopped: forget its keys and return them, so the caller
    /// can clear the stored values and notify the widgets.
    pub fn stop_dataset(&mut self, data_set_id: i64) -> Vec<String> {
        let mut keys: Vec<String> = self.owners.iter()
            .filter(|(_, &o)| o == data_set_id).map(|(k, _)| k.clone()).collect();
        keys.sort();
        for key in &keys {
            self.owners.remove(key);
            self.keys.remove(key);
        }
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(id: i64, schedule: i64, params: &str, md5: &str) -> ConnectorPayload {
        ConnectorPayload { data_set_id: id, schedule_id: schedule, data_params: params.into(),
                           js: format!("dataSet_{id}.js"), md5: md5.into() }
    }

    fn starts(commands: &[HostCommand]) -> Vec<i64> {
        commands.iter().filter_map(|c| match c { HostCommand::Start(p) => Some(p.data_set_id), _ => None }).collect()
    }

    fn stops(commands: &[HostCommand]) -> Vec<i64> {
        commands.iter().filter_map(|c| match c { HostCommand::Stop(id) => Some(*id), _ => None }).collect()
    }

    #[test]
    fn params_are_decoded_with_plus_as_space() {
        assert_eq!(decode_data_params("city%3DMassa+Carrara%26n%3D2"), "city=Massa Carrara&n=2");
        assert_eq!(decode_data_params(""), "");
    }

    #[test]
    fn payloads_skip_scripts_that_are_not_ready() {
        let xml = r#"<schedule><dataConnectors>
            <connector dataSetId="16" scheduleid="7" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" priority="0" js="dataSet_16.js" dataParams="a%3D1"/>
            <connector dataSetId="17" scheduleid="8" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" priority="0" js="dataSet_17.js" dataParams=""/>
        </dataConnectors></schedule>"#;
        let tree = elementtree::Element::from_reader(xml.as_bytes()).unwrap();
        let schedule = crate::schedule::Schedule::parse(&tree).unwrap();
        let entries = schedule.data_connectors_now(&crate::criteria::CriteriaStore::default());
        let list = payloads(&entries, |js| (js == "dataSet_16.js").then(|| "abc".to_string()));
        assert_eq!(list, [ConnectorPayload { data_set_id: 16, schedule_id: 7, data_params: "a%3D1".into(),
                                             js: "dataSet_16.js".into(), md5: "abc".into() }]);
    }

    #[test]
    fn a_wanted_connector_is_started_once() {
        let mut hosts = Hosts::default();
        let first = hosts.sync(vec![payload(16, 1, "", "m")]);
        assert_eq!(starts(&first), [16]);
        assert!(hosts.sync(vec![payload(16, 1, "", "m")]).is_empty(), "unchanged: nothing to do");
        assert_eq!(hosts.active_ids(), [16]);
    }

    #[test]
    fn a_connector_that_changes_is_restarted() {
        let mut hosts = Hosts::default();
        hosts.sync(vec![payload(16, 1, "", "m")]);
        for changed in [payload(16, 2, "", "m"), payload(16, 1, "x", "m"), payload(16, 1, "", "n")] {
            let mut h = Hosts::default();
            h.sync(vec![payload(16, 1, "", "m")]);
            let commands = h.sync(vec![changed]);
            assert_eq!(stops(&commands), [16]);
            assert_eq!(starts(&commands), [16]);
            assert!(matches!(commands[0], HostCommand::Stop(16)), "the stop comes first");
        }
    }

    #[test]
    fn an_unwanted_connector_stops_only_after_three_misses() {
        let mut hosts = Hosts::default();
        hosts.sync(vec![payload(16, 1, "", "m")]);
        assert!(hosts.sync(vec![]).is_empty());
        assert!(hosts.sync(vec![]).is_empty());
        assert_eq!(hosts.active_ids(), [16], "still running after two misses");
        assert_eq!(stops(&hosts.sync(vec![])), [16]);
        assert!(hosts.active_ids().is_empty());
    }

    #[test]
    fn being_wanted_again_resets_the_misses() {
        let mut hosts = Hosts::default();
        hosts.sync(vec![payload(16, 1, "", "m")]);
        hosts.sync(vec![]);
        hosts.sync(vec![]);
        assert!(hosts.sync(vec![payload(16, 1, "", "m")]).is_empty());
        hosts.sync(vec![]);
        hosts.sync(vec![]);
        assert!(hosts.sync(vec![]).len() == 1, "the count restarted from zero");
    }

    #[test]
    fn a_failed_start_is_retried_on_the_next_sync() {
        let mut hosts = Hosts::default();
        hosts.sync(vec![payload(16, 1, "", "m")]);
        hosts.forget(16);
        assert_eq!(starts(&hosts.sync(vec![payload(16, 1, "", "m")])), [16]);
    }

    #[test]
    fn several_connectors_are_handled_in_dataset_order() {
        let mut hosts = Hosts::default();
        let commands = hosts.sync(vec![payload(20, 1, "", "a"), payload(10, 1, "", "b")]);
        assert_eq!(starts(&commands), [10, 20]);
    }

    fn at(t0: Instant, ms: u64) -> Instant { t0 + Duration::from_millis(ms) }

    fn write(id: i64, key: &str, data: &str) -> Write {
        Write { data_set_id: id, key: key.into(), data: data.into() }
    }

    #[test]
    fn the_first_write_of_a_key_is_stored_immediately() {
        let mut w = RealtimeWriter::default();
        assert_eq!(w.set(Instant::now(), 16, "k", "v".into()), SetOutcome::Write(write(16, "k", "v")));
    }

    #[test]
    fn a_missing_key_is_rejected_with_400() {
        let mut w = RealtimeWriter::default();
        assert_eq!(w.set(Instant::now(), 16, "", "v".into()),
                   SetOutcome::Rejected { status: 400, message: "A dataKey is required" });
    }

    #[test]
    fn a_value_over_the_size_limit_is_rejected_with_413() {
        let mut w = RealtimeWriter::default();
        let big = "x".repeat(MAX_DATA_LENGTH + 1);
        assert_eq!(w.set(Instant::now(), 16, "k", big), SetOutcome::Rejected { status: 413, message: "Data too large" });
        let limit = "x".repeat(MAX_DATA_LENGTH);
        assert!(matches!(w.set(Instant::now(), 16, "k", limit), SetOutcome::Write(_)));
    }

    #[test]
    fn size_is_counted_in_utf16_units() {
        let mut w = RealtimeWriter::default();
        // 'a' + an emoji (2 units) repeated, just over the limit in units but far below it in chars
        let s: String = "\u{1F600}".repeat(MAX_DATA_LENGTH / 2 + 1);
        assert!(s.chars().count() < MAX_DATA_LENGTH);
        assert!(matches!(w.set(Instant::now(), 16, "k", s), SetOutcome::Rejected { status: 413, .. }));
    }

    #[test]
    fn the_hundred_and_first_key_of_a_dataset_is_rejected_with_429() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        for i in 0..MAX_KEYS_PER_DATASET {
            assert!(matches!(w.set(t0, 16, &format!("k{i}"), "v".into()), SetOutcome::Write(_)), "key {i}");
        }
        assert_eq!(w.set(t0, 16, "one-too-many", "v".into()),
                   SetOutcome::Rejected { status: 429, message: "Too many data keys" });
        // an existing key can still be written, and another dataset has its own budget
        assert!(!matches!(w.set(at(t0, 600), 16, "k0", "v2".into()), SetOutcome::Rejected { .. }));
        assert!(matches!(w.set(t0, 17, "other", "v".into()), SetOutcome::Write(_)));
    }

    #[test]
    fn a_rejected_key_is_not_left_owned() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        for i in 0..MAX_KEYS_PER_DATASET { w.set(t0, 16, &format!("k{i}"), "v".into()); }
        w.set(t0, 16, "extra", "v".into());
        assert_eq!(w.stop_dataset(16).len(), MAX_KEYS_PER_DATASET, "the rejected key was never owned");
    }

    #[test]
    fn writes_within_the_interval_are_coalesced_to_the_latest() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        assert!(matches!(w.set(t0, 16, "k", "1".into()), SetOutcome::Write(_)));
        assert_eq!(w.set(at(t0, 100), 16, "k", "2".into()), SetOutcome::Queued);
        assert_eq!(w.set(at(t0, 200), 16, "k", "3".into()), SetOutcome::Queued);
        assert!(w.due(at(t0, 499)).is_empty(), "not yet");
        assert_eq!(w.next_deadline(), Some(at(t0, 500)));
        let flushed = w.due(at(t0, 500));
        assert_eq!(flushed, [Flush { write: write(16, "k", "3"), notify: false }], "only the latest value is stored");
        assert_eq!(w.next_deadline(), None);
        assert!(w.due(at(t0, 2000)).is_empty());
    }

    #[test]
    fn a_write_after_the_interval_is_stored_immediately() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        w.set(t0, 16, "k", "1".into());
        assert!(matches!(w.set(at(t0, 500), 16, "k", "2".into()), SetOutcome::Write(_)));
    }

    #[test]
    fn keys_are_throttled_independently() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        w.set(t0, 16, "a", "1".into());
        assert!(matches!(w.set(at(t0, 10), 16, "b", "1".into()), SetOutcome::Write(_)));
    }

    #[test]
    fn notify_is_immediate_unless_a_write_is_pending() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        assert_eq!(w.notify("nothing-written"), NotifyAction::Now);
        w.set(t0, 16, "k", "1".into());
        assert_eq!(w.notify("k"), NotifyAction::Now);
        w.set(at(t0, 100), 16, "k", "2".into());
        assert_eq!(w.notify("k"), NotifyAction::WhenWritten);
        let flushed = w.due(at(t0, 500));
        assert_eq!(flushed.len(), 1);
        assert!(flushed[0].notify, "the waiting notification goes out with the write");
        w.set(at(t0, 1200), 16, "k", "3".into());
        w.set(at(t0, 1300), 16, "k", "4".into());
        assert!(!w.due(at(t0, 1700))[0].notify, "and is not repeated");
    }

    #[test]
    fn stopping_a_connector_returns_and_forgets_its_keys() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        w.set(t0, 16, "b", "1".into());
        w.set(t0, 16, "a", "1".into());
        w.set(t0, 17, "c", "1".into());
        w.set(at(t0, 10), 16, "a", "2".into()); // queued
        assert_eq!(w.stop_dataset(16), ["a", "b"]);
        assert!(w.due(at(t0, 1000)).is_empty(), "queued writes of the stopped connector are dropped");
        assert_eq!(w.stop_dataset(16), Vec::<String>::new());
        assert_eq!(w.stop_dataset(17), ["c"]);
    }

    #[test]
    fn a_key_written_by_two_connectors_goes_to_the_last_writer() {
        let mut w = RealtimeWriter::default();
        let t0 = Instant::now();
        w.set(t0, 16, "shared", "1".into());
        w.set(at(t0, 600), 17, "shared", "2".into());
        assert_eq!(w.stop_dataset(16), Vec::<String>::new(), "no longer owned by 16");
        assert_eq!(w.stop_dataset(17), ["shared"]);
    }
}
