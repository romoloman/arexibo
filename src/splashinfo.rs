//! What the splash screen shows for debugging, served at `/splash-info`
//! (local requests only): the version, the free and total space of the
//! cache's disk, and when the last collection was and how it went. Apart
//! from `/info`, which keeps the Electron player's fields exactly.

use std::path::PathBuf;
use std::sync::Mutex;
use once_cell::sync::Lazy;
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectResult {
    Running,
    Ok,
    Failed,
}

impl CollectResult {
    fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Default)]
struct State {
    disk_path: Option<PathBuf>,
    last_collect: Option<(OffsetDateTime, CollectResult)>,
}

impl State {
    fn collect_started(&mut self, at: OffsetDateTime) {
        self.last_collect = Some((at, CollectResult::Running));
    }

    fn collect_finished(&mut self, ok: bool, at: OffsetDateTime) {
        self.last_collect = Some((at, if ok { CollectResult::Ok } else { CollectResult::Failed }));
    }
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplashInfo {
    pub version: String,
    pub disk_free_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    /// RFC 3339, to the second.
    pub last_collect_at: Option<String>,
    /// `running`, `ok` or `failed`.
    pub last_collect_result: Option<String>,
}

static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::default()));

fn update(f: impl FnOnce(&mut State)) {
    f(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()));
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
}

/// The directory whose disk is reported (the cache's).
pub fn set_disk_path(path: PathBuf) {
    update(|state| state.disk_path = Some(path));
}

pub fn collect_started() {
    update(|state| state.collect_started(now()));
}

pub fn collect_finished(ok: bool) {
    update(|state| state.collect_finished(ok, now()));
}

fn snapshot(state: &State) -> SplashInfo {
    let disk = state.disk_path.as_deref().and_then(|path| crate::util::space_info(path).ok());
    SplashInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        disk_free_bytes: disk.map(|(free, _)| free),
        disk_total_bytes: disk.map(|(_, total)| total),
        last_collect_at: state.last_collect.and_then(|(at, _)| {
            at.replace_nanosecond(0).ok().and_then(|at| at.format(&Rfc3339).ok())
        }),
        last_collect_result: state.last_collect.map(|(_, result)| result.name().to_string()),
    }
}

pub fn json() -> String {
    let info = snapshot(&STATE.lock().unwrap_or_else(|e| e.into_inner()));
    serde_json::to_string(&info).expect("serializing splash info")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).unwrap()
    }

    #[test]
    fn the_json_has_the_fields_the_splash_reads() {
        let v: Value = serde_json::from_str(&json()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["diskFreeBytes", "diskTotalBytes", "lastCollectAt", "lastCollectResult", "version"]);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn nothing_is_known_before_the_first_collection_or_a_directory() {
        let info = snapshot(&State::default());
        assert_eq!((info.disk_free_bytes, info.disk_total_bytes), (None, None));
        assert_eq!((info.last_collect_at, info.last_collect_result), (None, None));
    }

    #[test]
    fn the_disk_figures_are_those_of_the_directory() {
        let state = State { disk_path: Some(std::env::temp_dir()), last_collect: None };
        let info = snapshot(&state);
        let (free, total) = (info.disk_free_bytes.unwrap(), info.disk_total_bytes.unwrap());
        assert!(total > 0 && free <= total, "{free} of {total}");
    }

    #[test]
    fn a_directory_that_is_not_there_has_no_figures() {
        let state = State { disk_path: Some(PathBuf::from("/nonexistent/arexibo")), last_collect: None };
        assert_eq!(snapshot(&state).disk_free_bytes, None);
    }

    #[test]
    fn a_collection_is_running_and_then_it_worked_or_failed() {
        let mut state = State::default();
        state.collect_started(at("2026-10-23T12:46:02+02:00"));
        let info = snapshot(&state);
        assert_eq!(info.last_collect_at.as_deref(), Some("2026-10-23T12:46:02+02:00"));
        assert_eq!(info.last_collect_result.as_deref(), Some("running"));
        state.collect_finished(true, at("2026-10-23T12:46:09+02:00"));
        let info = snapshot(&state);
        assert_eq!(info.last_collect_at.as_deref(), Some("2026-10-23T12:46:09+02:00"));
        assert_eq!(info.last_collect_result.as_deref(), Some("ok"));
        state.collect_started(at("2026-10-23T12:51:02+02:00"));
        state.collect_finished(false, at("2026-10-23T12:51:05+02:00"));
        assert_eq!(snapshot(&state).last_collect_result.as_deref(), Some("failed"));
    }

    #[test]
    fn the_time_has_no_fraction_of_a_second() {
        let mut state = State::default();
        state.collect_started(at("2026-10-23T12:46:02.987654321+02:00"));
        assert_eq!(snapshot(&state).last_collect_at.as_deref(), Some("2026-10-23T12:46:02+02:00"));
    }
}
