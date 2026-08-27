// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Schedule parsing and scheduling.

use std::{fs::File, path::Path};
use anyhow::{Context, Result};
use time::{OffsetDateTime, PrimitiveDateTime};
use elementtree::Element;
use serde::{Serialize, Deserialize};
use crate::criteria::{criterion_matches, CriteriaStore};
use crate::resource::LayoutId;
use crate::util::{TIME_FMT, ElementExt};

/// A single `<criteria metric="" condition="" type="...">value</criteria>`
/// node attached to a schedule item (Xibo 4.1+, Schedule Criteria) --
/// `type` (e.g. "weather") is a CMS-side UI hint about where the metric
/// comes from, not needed for evaluation, so it isn't captured here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleCriterion {
    pub metric: String,
    pub condition: String,
    pub value: String,
}

/// A single `<action>` node inside `<actions>` -- confirmed real from a
/// live CMS: `<action fromdt="" todt="" scheduleid="" priority="" duration=""
/// isGeoAware="" geoLocation="" triggerCode="" actionType="" layoutCode=""
/// commandCode=""/>`. Distinct from both ScheduledCommand (time-triggered,
/// no incoming code needed) and a widget-embedded `<action
/// triggerType="webhook">` (only reachable while its own layout/widget is
/// already on screen) -- this is the CMS's own official manual concept:
/// "Scheduled Actions listen for a Trigger Code coming in on a webhook to
/// Navigate to a Layout or to run a Command" -- reachable regardless of
/// what's currently showing, confirmed to match a real user-reported
/// symptom (a trigger doing nothing because the *current* layout had no
/// matching widget-embedded action at all).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActionTarget {
    /// `actionType="navLayout"` -- `layoutCode` resolved via the same
    /// code_map already used for a widget-embedded action's own
    /// layoutCode-based navigation (see resource.rs's own
    /// resolve_layout_code).
    Layout(String),
    /// `actionType="command"` -- `commandCode` matches a key in
    /// PlayerSettings::commands, same as ScheduledCommand's own `code`.
    Command(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ScheduledAction {
    from: OffsetDateTime,
    to: OffsetDateTime,
    scheduleid: i64,
    /// Seconds. 0 means "no fixed duration" -- confirmed real semantics,
    /// user-specified: >0 reverts to the normal schedule after this many
    /// seconds; 0 instead waits for the shown layout's own natural
    /// completion (its own region/widget cycle finishing once) before
    /// reverting -- see mainloop.rs's own handle_trigger_code and
    /// FromGui::LayoutCompleted.
    duration: i64,
    trigger_code: String,
    target: ActionTarget,
}

/// A single `<command date="" scheduleid="" code=""/>` node -- a
/// scheduled, time-triggered invocation of a Command (the command's own
/// *definition*, what it actually does, comes from RegisterDisplay's own
/// `<commands>`/PlayerSettings::commands; this only says *when* to run
/// one). `date` is a single instant to fire at, not a fromdt/todt window.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduledCommand {
    date: OffsetDateTime,
    scheduleid: i64,
    code: String,
}

/// A single `<overlay file="" fromdt="" todt="" scheduleid="" priority=""
/// duration="" isGeoAware="" geoLocation="" maxPlaysPerHour=""/>` node
/// inside the schedule's own `<overlays>` wrapper -- a separate element
/// from `<layout>`, sibling to normal schedule entries. Corresponds to
/// Xibo's "Overlay Layout" Event Type: shown on top of the normal
/// schedule, rotating if more than one is active. Distinct from (but
/// reuses the same mechanism as) the XMR `overlayLayout` push action
/// (mainloop.rs) -- this is the persistent, schedule-driven source, XMR
/// is the transient, CMS-pushed one. No shareOfVoice/cyclePlayback/
/// playCount/syncEvent -- only a plain per-item `duration` for rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OverlayEntry {
    from: OffsetDateTime,
    to: OffsetDateTime,
    layoutid: LayoutId,
    duration: i64,
    priority: i32,
    scheduleid: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduleEntry {
    from: OffsetDateTime,
    to: OffsetDateTime,
    layoutid: LayoutId,
    priority: i32,
    // Needed for Proof of Play stat records (see stats.rs), which the
    // CMS requires alongside the layout id for every "layout" record.
    scheduleid: i64,
    // Schedule Criteria (see criteria.rs) -- ALL of these must currently
    // be satisfied (AND semantics, matching the C# client's
    // `isAllCriteriaActive`) for this entry to count as active in
    // `layouts_now()`. Empty means "no criteria conditioning, always
    // eligible" (the overwhelmingly common case).
    criteria: Vec<ScheduleCriterion>,
    // Xibo Interrupt Layouts / Share of Voice (v5+, CMS 2.2+): >0 marks
    // this schedule entry as an Interrupt Layout, value is its target
    // percentage of screen time within each hour -- see
    // `resolve_normal_and_interrupts`. FLAGGED AS UNVERIFIED attribute
    // name ("shareOfVoice") -- inferred from the official docs' own
    // repeated use of that exact term for this exact concept, following
    // the usual attribute-naming convention, but not independently
    // confirmed against a real Schedule XML payload.
    #[serde(default)]
    share_of_voice: i32,
    // Per-schedule-item duration override in seconds, if the CMS
    // provides one -- confirmed to exist in principle ("from 2.3.10 CMS
    // this is provided in XMDS", per the real xibo-dotnetclient source),
    // but the exact attribute name is FLAGGED AS UNVERIFIED (assumed
    // "duration"). None falls back to a flat 60s default -- same
    // literal fallback constant the C# client itself uses when it too
    // has no better number (it additionally falls back to a cached
    // historical actual-play-duration before that flat 60s; arexibo
    // does not maintain that cache, a deliberate scope simplification).
    #[serde(default)]
    duration: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Schedule {
    default: Option<LayoutId>,
    schedules: Vec<ScheduleEntry>,
    #[serde(default)]
    overlays: Vec<OverlayEntry>,
    #[serde(default)]
    commands: Vec<ScheduledCommand>,
    #[serde(default)]
    actions: Vec<ScheduledAction>,
}

impl Schedule {
    pub fn parse(tree: &Element) -> Result<Self> {
        let tz_offset = OffsetDateTime::now_local().unwrap().offset();
        let mut schedules = Vec::new();
        for layout in tree.find_all("layout") {
            let layoutid = layout.parse_attr("file")?;
            let priority = layout.parse_attr("priority")?;
            // `#[serde(default)]`-equivalent fallback to 0 -- seen as a
            // real attribute on live schedule XML (confirmed via CMS
            // community reports: `scheduleid="109"`), but default to 0
            // defensively in case some schedule entry (e.g. an XMR
            // changeLayout override, if the CMS ever synthesizes one into
            // the regular schedule XML rather than only via XMR) omits it
            // -- matches the "0" convention several other player
            // implementations use for a non-scheduled/default layout play.
            let scheduleid = layout.get_attr("scheduleid")
                .and_then(|s| s.parse().ok()).unwrap_or(0);
            let from = layout.get_attr("fromdt").context("missing fromdt")?;
            let to = layout.get_attr("todt").context("missing todt")?;
            let from = PrimitiveDateTime::parse(from, &TIME_FMT)
                .context("invalid fromdt")?
                .assume_offset(tz_offset);
            let to = PrimitiveDateTime::parse(to, &TIME_FMT)
                .context("invalid todt")?
                .assume_offset(tz_offset);
            // FLAGGED AS UNVERIFIED (see criteria.rs's own doc comment
            // for the `condition` attribute specifically): confirmed via
            // official docs that a `<criteria metric="" condition=""
            // type="...">value</criteria>` node exists as a direct child
            // of the schedule's `<layout>` node, one or more per entry.
            let criteria = layout.find_all("criteria").map(|c| Ok(ScheduleCriterion {
                metric: c.get_attr("metric").context("criteria missing metric")?.into(),
                condition: c.get_attr("condition").context("criteria missing condition")?.into(),
                value: c.text().into(),
            })).collect::<Result<Vec<_>>>()?;
            let share_of_voice = layout.get_attr("shareOfVoice")
                .and_then(|s| s.parse().ok()).unwrap_or(0);
            let duration = layout.get_attr("duration").and_then(|s| s.parse().ok());
            schedules.push(ScheduleEntry {
                from, to, layoutid, priority, scheduleid, criteria, share_of_voice, duration,
            });
        }
        let mut default = None;
        if let Some(def) = tree.find("default") {
            default = Some(def.parse_attr("file")?);
        }

        // Confirmed real structure from a real schedule.xml the user
        // shared: a top-level `<overlays>` wrapper (sibling to the
        // `<layout>` entries and `<default>`), containing `<overlay>`
        // children -- see OverlayEntry's own doc comment for the full
        // story. Entirely optional/absent when no Overlay Layout is
        // currently scheduled, hence `tree.find` (not required) here.
        let mut overlays = Vec::new();
        if let Some(overlays_el) = tree.find("overlays") {
            for overlay in overlays_el.find_all("overlay") {
                let layoutid = overlay.parse_attr("file")?;
                let priority = overlay.parse_attr("priority")?;
                let scheduleid = overlay.get_attr("scheduleid")
                    .and_then(|s| s.parse().ok()).unwrap_or(0);
                let from = overlay.get_attr("fromdt").context("overlay missing fromdt")?;
                let to = overlay.get_attr("todt").context("overlay missing todt")?;
                let from = PrimitiveDateTime::parse(from, &TIME_FMT)
                    .context("overlay invalid fromdt")?
                    .assume_offset(tz_offset);
                let to = PrimitiveDateTime::parse(to, &TIME_FMT)
                    .context("overlay invalid todt")?
                    .assume_offset(tz_offset);
                // Confirmed real (`duration="91"` in the real example) --
                // a schedule-level duration in seconds, distinct from any
                // XLF-declared per-widget duration, used only for
                // rotation timing between multiple simultaneously-active
                // overlays (see `active_overlays`). Falls back to 60s
                // (same convention used elsewhere in this file) if
                // somehow absent or non-positive.
                let duration = overlay.get_attr("duration")
                    .and_then(|s| s.parse::<i64>().ok())
                    .filter(|&d| d > 0)
                    .unwrap_or(60);
                // `isGeoAware`/`geoLocation` are real attributes here too,
                // but deliberately not acted upon -- GeoAware filtering
                // isn't implemented anywhere in this file for normal
                // layouts either (arexibo has no geolocation source at
                // all, matching the same scope note in adspace.rs's own
                // `geo: None`), so overlays are treated no differently:
                // shown regardless of any GeoAware conditioning, rather
                // than silently hidden by a check that doesn't actually
                // exist.
                overlays.push(OverlayEntry { from, to, layoutid, duration, priority, scheduleid });
            }
        }

        // Scheduled command invocations -- `<command date="" scheduleid=""
        // code=""/>`, confirmed real from a live CMS (a "TESTTOUCH" command
        // repeated hourly over several days). `code` matches a key in
        // PlayerSettings::commands (the *definition* of what the command
        // actually does, from RegisterDisplay) -- this element only says
        // *when* to run it. `date` is a single instant, not a fromdt/todt
        // window like layouts/overlays -- see the reference client's own
        // `command.Date >= now && command.Date < tenSecondsTime &&
        // !command.HasRun` (xibo-dotnetclient/Logic/ScheduleManager.cs) for
        // the due-check semantics this is meant to support: a narrow
        // forward-looking window, checked once, not "any time in the past
        // we've never seen this before" (confirmed via a real historical
        // bug, xibo/xibo#1327, fixed by exactly this kind of once-only
        // tracking -- a command re-firing repeatedly, or firing long after
        // its own scheduled time following an outage, is exactly the
        // failure mode being guarded against here).
        let mut commands = Vec::new();
        for command in tree.find_all("command") {
            let scheduleid = command.get_attr("scheduleid")
                .and_then(|s| s.parse().ok()).unwrap_or(0);
            let code = command.get_attr("code").context("command missing code")?.to_string();
            let date = command.get_attr("date").context("command missing date")?;
            let date = PrimitiveDateTime::parse(date, &TIME_FMT)
                .context("command invalid date")?
                .assume_offset(tz_offset);
            commands.push(ScheduledCommand { date, scheduleid, code });
        }

        // Scheduled Actions -- `<actions><action .../></actions>`,
        // confirmed real from a live CMS (a "navLayout" action, trigger
        // code "apri_home", targeting a layout by its own layoutCode).
        // See ScheduledAction's own doc comment for the full reasoning.
        let mut actions = Vec::new();
        if let Some(actions_el) = tree.find("actions") {
            for action in actions_el.find_all("action") {
                let scheduleid = action.get_attr("scheduleid")
                    .and_then(|s| s.parse().ok()).unwrap_or(0);
                let duration = action.get_attr("duration")
                    .and_then(|s| s.parse().ok()).unwrap_or(0);
                let trigger_code = action.get_attr("triggerCode")
                    .context("action missing triggerCode")?.to_string();
                let action_type = action.get_attr("actionType")
                    .context("action missing actionType")?;
                let target = match action_type {
                    "navLayout" => ActionTarget::Layout(
                        action.get_attr("layoutCode")
                            .context("navLayout action missing layoutCode")?.to_string()),
                    "command" => ActionTarget::Command(
                        action.get_attr("commandCode")
                            .context("command action missing commandCode")?.to_string()),
                    other => {
                        // Forward-compatible: skip an actionType we don't
                        // recognize rather than failing the whole
                        // schedule parse over it.
                        log::warn!("skipping scheduled action with unknown actionType {other:?}");
                        continue;
                    }
                };
                let from = action.get_attr("fromdt").context("action missing fromdt")?;
                let from = PrimitiveDateTime::parse(from, &TIME_FMT)
                    .context("action invalid fromdt")?.assume_offset(tz_offset);
                let to = action.get_attr("todt").context("action missing todt")?;
                let to = PrimitiveDateTime::parse(to, &TIME_FMT)
                    .context("action invalid todt")?.assume_offset(tz_offset);
                actions.push(ScheduledAction { from, to, scheduleid, duration, trigger_code, target });
            }
        }

        Ok(Self {
            default,
            schedules,
            overlays,
            commands,
            actions,
        })
    }

    /// Currently-active Overlay Layouts (see `OverlayEntry`'s own doc
    /// comment), as (layoutid, duration_secs) pairs in schedule order --
    /// zero, one, or several can be simultaneously active (the caller,
    /// mainloop.rs, is responsible for cycling through more than one
    /// using each entry's own `duration`, mirroring how the official
    /// client rotates through multiple concurrently-scheduled overlays).
    /// Unlike normal layouts, overlays are NOT reduced to only the
    /// highest priority here -- a deliberate simplification (priority
    /// semantics specifically for overlays weren't independently
    /// confirmed, and showing "too many" overlays is a much safer
    /// failure mode than silently hiding one that should be visible).
    pub fn active_overlays(&self) -> Vec<(LayoutId, i64)> {
        let now = OffsetDateTime::now_local().unwrap();
        self.overlays.iter()
            .filter(|o| o.from <= now && now <= o.to)
            .map(|o| (o.layoutid, o.duration))
            .collect()
    }

    /// Look up the scheduleid for a currently-active overlay layout --
    /// same purpose/convention as `scheduleid_for` above (Proof of Play),
    /// just for the separate overlays list.
    pub fn overlay_scheduleid_for(&self, layoutid: LayoutId) -> i64 {
        let now = OffsetDateTime::now_local().unwrap();
        for o in &self.overlays {
            if o.layoutid == layoutid && o.from <= now && now <= o.to {
                return o.scheduleid;
            }
        }
        0
    }

    /// Layouts that should be showing right now. Without any active
    /// Interrupt Layout (`shareOfVoice > 0`), this is just the
    /// highest-active-priority normal layouts (or the default, if
    /// none) -- same as before Share of Voice support existed. With one
    /// or more active interrupts, returns a full resolved one-hour
    /// sequence instead (see `resolve_normal_and_interrupts`) -- the
    /// caller (gui.rs's cycling `Schedule<T>`) already knows how to
    /// advance through and wrap around an arbitrary-length sequence, so
    /// no changes are needed there to support the (possibly much
    /// longer, with repeated entries) sequence this can now return.
    pub fn layouts_now(&self, criteria: &CriteriaStore) -> Vec<LayoutId> {
        let now = OffsetDateTime::now_local().unwrap();
        let active: Vec<&ScheduleEntry> = self.schedules.iter()
            .filter(|e| e.from <= now && now <= e.to && self.criteria_satisfied(e, criteria))
            .collect();

        let normal_entries = Self::highest_priority(
            active.iter().copied().filter(|e| e.share_of_voice <= 0));
        let interrupt_entries = Self::highest_priority(
            active.iter().copied().filter(|e| e.share_of_voice > 0));

        if interrupt_entries.is_empty() {
            let mut layouts: Vec<LayoutId> = normal_entries.iter().map(|e| e.layoutid).collect();
            if layouts.is_empty() {
                if let Some(def) = self.default {
                    layouts.push(def);
                }
            }
            return layouts;
        }

        const DEFAULT_DURATION: i64 = 60;
        let mut normal: Vec<(LayoutId, i64)> = normal_entries.iter()
            .map(|e| (e.layoutid, e.duration.filter(|&d| d > 0).unwrap_or(DEFAULT_DURATION)))
            .collect();
        if normal.is_empty() {
            // Matches the C#'s own fallback to its "current default
            // layout" when there's no normal-priority schedule to fill
            // the remaining (non-interrupt) time with.
            if let Some(def) = self.default {
                normal.push((def, DEFAULT_DURATION));
            }
        }
        let interrupt: Vec<(LayoutId, i64, i32)> = interrupt_entries.iter()
            .map(|e| (e.layoutid, e.duration.filter(|&d| d > 0).unwrap_or(DEFAULT_DURATION),
                      e.share_of_voice))
            .collect();

        resolve_normal_and_interrupts(&normal, &interrupt)
    }

    fn highest_priority<'a>(items: impl Iterator<Item = &'a ScheduleEntry>) -> Vec<&'a ScheduleEntry> {
        let mut highest = 0;
        let mut resolved = Vec::new();
        for item in items {
            if item.priority > highest {
                resolved.clear();
                highest = item.priority;
            }
            if item.priority == highest {
                resolved.push(item);
            }
        }
        resolved
    }

    fn criteria_satisfied(&self, entry: &ScheduleEntry, criteria: &CriteriaStore) -> bool {
        entry.criteria.iter().all(|c| {
            match criteria.get(&c.metric) {
                Some(actual) => criterion_matches(&c.condition, &c.value, actual),
                // A criterion whose metric has never been set (or has
                // expired) is not satisfied -- fail closed, don't show a
                // criteria-conditioned layout just because we haven't
                // heard from whatever's supposed to set it.
                None => false,
            }
        })
    }

    /// Look up the scheduleid for a layout that's currently active (i.e.
    /// `now` falls within one of its scheduled from/to windows) -- used
    /// to attach the right scheduleid to a Proof of Play "layout" record
    /// when that layout starts showing (see mainloop.rs). Returns 0 (the
    /// "no real schedule entry" convention -- e.g. the default layout,
    /// which isn't a `<layout>` schedule entry at all) if no active
    /// schedule entry for this layout id is found.
    pub fn scheduleid_for(&self, layoutid: LayoutId) -> i64 {
        let now = OffsetDateTime::now_local().unwrap();
        for entry in &self.schedules {
            if entry.layoutid == layoutid && entry.from <= now && now <= entry.to {
                return entry.scheduleid;
            }
        }
        0
    }

    /// Scheduled commands (see ScheduledCommand's own doc comment) due
    /// right now -- as (scheduleid, code) pairs, in schedule order.
    /// `already_run` is this session's own in-memory set of scheduleids
    /// already fired (see mainloop.rs's own commands_run field) --
    /// checked here rather than mutating any state on `self`, since
    /// `Schedule` itself gets replaced wholesale on every collection
    /// cycle (see mainloop.rs's own `self.schedule = ...`) and wouldn't
    /// remember having fired something from one cycle to the next
    /// otherwise. Same due-check semantics as the reference client's own
    /// `command.Date >= now && command.Date < tenSecondsTime &&
    /// !command.HasRun` (see ScheduledCommand's own doc comment for the
    /// full reasoning) -- a narrow forward-looking window, not "any time
    /// in the past we've never seen this before".
    pub fn commands_due(&self, now: OffsetDateTime,
                         already_run: &std::collections::HashSet<i64>) -> Vec<(i64, String)> {
        const WINDOW: time::Duration = time::Duration::seconds(10);
        self.commands.iter()
            .filter(|c| now >= c.date && now < c.date + WINDOW
                        && !already_run.contains(&c.scheduleid))
            .map(|c| (c.scheduleid, c.code.clone()))
            .collect()
    }

    /// The Scheduled Action (see ScheduledAction's own doc comment)
    /// matching `code`, if any is currently valid (`from <= now <= to`).
    /// Returns (duration, target) -- see mainloop.rs's own
    /// handle_trigger_code for how these get used. If more than one
    /// currently-valid action shares the same trigger code (not expected
    /// in practice, but not something the CMS itself seems to prevent),
    /// the first match wins -- no documented tie-breaking rule to follow
    /// here.
    pub fn action_for_trigger(&self, code: &str, now: OffsetDateTime) -> Option<(i64, &ActionTarget)> {
        self.actions.iter()
            .find(|a| a.trigger_code == code && a.from <= now && now <= a.to)
            .map(|a| (a.duration, &a.target))
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        serde_json::from_reader(File::open(path.as_ref())?)
            .context("deserializing schedule")
    }

    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        serde_json::to_writer_pretty(File::create(path.as_ref())?, self)
            .context("serializing schedule")
    }
}

/// Faithful (bug-fixed) port of the C# client's
/// `ResolveNormalAndInterrupts`/`ParseCyclePlayback` (Logic/
/// ScheduleManager.cs) -- combines normal-priority and Interrupt
/// Layouts (`shareOfVoice > 0`) into one ordered sequence for an hour's
/// worth of plays, interrupts spread proportionally to their share of
/// voice, normal layouts round-robining through the rest.
///
/// NOT reproducing a real bug in that source: it over-subtracts the
/// Adspace Exchange share of voice using a cumulative running total
/// instead of the final total once. Moot here (no AXE integration
/// yet) -- noted for whenever that gets built.
///
/// Deliberately not ported: AXE reduction itself, `MaxPlaysPerHour`
/// (unrelated per-item play-count cap), and the C#'s historical
/// actual-play-duration cache -- duration here is always the
/// schedule-item's own `duration` or a 60s fallback (see `layouts_now`).
fn resolve_normal_and_interrupts(
    normal: &[(LayoutId, i64)],
    interrupt: &[(LayoutId, i64, i32)],
) -> Vec<LayoutId> {
    const HOUR_SECS: i64 = 3600;

    // Each interrupt layout accumulates committed seconds (cycling
    // through the interrupt list repeatedly, not just once) until it
    // individually reaches its own target: shareOfVoice% of the hour.
    let targets: Vec<i64> = interrupt.iter().map(|&(_, _, sov)| (sov as i64 * HOUR_SECS) / 100).collect();
    let mut committed = vec![0i64; interrupt.len()];
    let mut resolved_interrupt: Vec<(LayoutId, i64)> = Vec::new();
    let mut interrupt_secs = 0i64;
    if !interrupt.is_empty() {
        let mut idx = 0;
        loop {
            if idx >= interrupt.len() {
                idx = 0;
                if committed.iter().zip(&targets).all(|(&c, &t)| c >= t) {
                    break;
                }
            }
            if committed[idx] < targets[idx] {
                let (id, dur, _) = interrupt[idx];
                committed[idx] += dur;
                interrupt_secs += dur;
                resolved_interrupt.push((id, dur));
            }
            idx += 1;
        }
    }

    // If the interrupt schedule alone already fills (or exceeds) the
    // whole hour, just cycle the raw interrupt list forever -- no
    // normal layouts are needed at all.
    if interrupt_secs >= HOUR_SECS {
        return interrupt.iter().map(|&(id, _, _)| id).collect();
    }

    // Fill the remaining time with normal layouts, round-robin -- unlike
    // interrupts, normal layouts don't have an individual percentage
    // target to hit, they just take equal turns.
    let mut normal_secs_remaining = HOUR_SECS - interrupt_secs;
    let mut resolved_normal: Vec<(LayoutId, i64)> = Vec::new();
    let mut nidx = 0;
    while normal_secs_remaining > 0 && !normal.is_empty() {
        if nidx >= normal.len() {
            nidx = 0;
        }
        let (id, dur) = normal[nidx];
        let dur = dur.max(10); // guard against a zero/negative duration
        normal_secs_remaining -= dur;
        resolved_normal.push((id, dur));
        nidx += 1;
    }

    if resolved_normal.is_empty() {
        // Only possible if `normal` itself was empty (no default layout
        // configured either) -- fall back to just the interrupts.
        return resolved_interrupt.into_iter().map(|(id, _)| id).collect();
    }

    // Interleave: spread interrupts evenly among the normal layouts
    // rather than clustering them all together. Ceiling for normal
    // (never starve it), floor for interrupt (never overpick it).
    let pick_count = resolved_normal.len().max(resolved_interrupt.len());
    let normal_pick = pick_count.div_ceil(resolved_normal.len());
    let interrupt_pick = if resolved_interrupt.is_empty() {
        pick_count + 1 // never triggers, i % (pick_count+1) == 0 only at i=0, guarded by the index check anyway
    } else {
        (pick_count / resolved_interrupt.len()).max(1)
    };

    let mut resolved = Vec::new();
    let mut normal_i = 0usize;
    let mut interrupt_i = 0usize;
    let mut total_secs = 0i64;
    for i in 0..pick_count {
        if i % normal_pick == 0 {
            if normal_i >= resolved_normal.len() {
                normal_i = 0;
            }
            let (id, dur) = resolved_normal[normal_i];
            resolved.push(id);
            total_secs += dur;
            normal_i += 1;
        }
        if i % interrupt_pick == 0 && interrupt_i < resolved_interrupt.len() {
            let (id, dur) = resolved_interrupt[interrupt_i];
            resolved.push(id);
            total_secs += dur;
            interrupt_i += 1;
        }
    }

    // Rounding (ceil/floor picks) can leave a small gap at the end --
    // top it up with more normal layouts, continuing the round-robin
    // (matches xibo-dotnetclient issue #263, which this same fix
    // addresses in the real client too).
    while total_secs < HOUR_SECS {
        if normal_i >= resolved_normal.len() {
            normal_i = 0;
        }
        let (id, dur) = resolved_normal[normal_i];
        resolved.push(id);
        total_secs += dur;
        normal_i += 1;
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(layoutid: LayoutId, priority: i32, criteria: Vec<ScheduleCriterion>) -> ScheduleEntry {
        let now = OffsetDateTime::now_local().unwrap();
        ScheduleEntry {
            from: now - time::Duration::hours(1),
            to: now + time::Duration::hours(1),
            layoutid, priority, scheduleid: 1, criteria,
            share_of_voice: 0, duration: None,
        }
    }

    fn interrupt_entry(layoutid: LayoutId, share_of_voice: i32, duration: i64) -> ScheduleEntry {
        let mut e = entry(layoutid, 0, vec![]);
        e.share_of_voice = share_of_voice;
        e.duration = Some(duration);
        e
    }

    #[test]
    fn layout_with_no_criteria_is_always_eligible() {
        let sched = Schedule { default: None, schedules: vec![entry(1, 0, vec![])], overlays: vec![], commands: vec![], actions: vec![] };
        assert_eq!(sched.layouts_now(&CriteriaStore::default()), vec![1]);
    }

    #[test]
    fn layout_with_unsatisfied_criteria_is_excluded() {
        let crit = ScheduleCriterion {
            metric: "temperature".into(), condition: "gt".into(), value: "30".into(),
        };
        let sched = Schedule { default: Some(99), schedules: vec![entry(1, 0, vec![crit])], overlays: vec![], commands: vec![], actions: vec![] };
        // no criteria set at all -> fails closed, falls back to default
        assert_eq!(sched.layouts_now(&CriteriaStore::default()), vec![99]);

        let mut cs = CriteriaStore::default();
        cs.set("temperature".into(), "20".into(), 3600); // below the gt 30 threshold
        assert_eq!(sched.layouts_now(&cs), vec![99]);
    }

    #[test]
    fn layout_with_satisfied_criteria_is_included() {
        let crit = ScheduleCriterion {
            metric: "temperature".into(), condition: "gt".into(), value: "30".into(),
        };
        let sched = Schedule { default: Some(99), schedules: vec![entry(1, 0, vec![crit])], overlays: vec![], commands: vec![], actions: vec![] };
        let mut cs = CriteriaStore::default();
        cs.set("temperature".into(), "35".into(), 3600);
        assert_eq!(sched.layouts_now(&cs), vec![1]);
    }

    #[test]
    fn all_criteria_must_match_and_semantics() {
        let crit_ok = ScheduleCriterion {
            metric: "temperature".into(), condition: "gt".into(), value: "30".into(),
        };
        let crit_fail = ScheduleCriterion {
            metric: "weather_condition".into(), condition: "eq".into(), value: "rain".into(),
        };
        let sched = Schedule {
            default: Some(99),
            schedules: vec![entry(1, 0, vec![crit_ok, crit_fail])],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        let mut cs = CriteriaStore::default();
        cs.set("temperature".into(), "35".into(), 3600);
        cs.set("weather_condition".into(), "clear".into(), 3600); // doesn't match "rain"
        // one of two criteria fails -> whole entry excluded
        assert_eq!(sched.layouts_now(&cs), vec![99]);
    }

    #[test]
    fn no_interrupts_behaves_exactly_as_before() {
        let sched = Schedule {
            default: None,
            schedules: vec![entry(1, 0, vec![]), entry(2, 0, vec![])],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        let result = sched.layouts_now(&CriteriaStore::default());
        assert_eq!(result, vec![1, 2]);
    }

    #[test]
    fn interrupt_fills_whole_hour_alone() {
        // A single interrupt at 100% SOV with a 3600s duration should
        // just cycle the raw interrupt list, no normal layouts needed.
        let sched = Schedule {
            default: None,
            schedules: vec![
                entry(1, 0, vec![]),
                interrupt_entry(2, 100, 3600),
            ],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        assert_eq!(sched.layouts_now(&CriteriaStore::default()), vec![2]);
    }

    #[test]
    fn interrupt_spread_across_normal_layout() {
        // Normal layout: 600s duration. Interrupt: 10% SOV (target 360s),
        // 60s duration -> needs 6 plays to satisfy its target.
        // Normal needs to fill 3600-360=3240s at 600s each -> 6 plays
        // (3600s, slight overshoot from round-robin fill, matches C#
        // behavior of possibly overshooting slightly rather than
        // undershooting).
        let sched = Schedule {
            default: None,
            schedules: vec![
                {
                    let mut e = entry(1, 0, vec![]);
                    e.duration = Some(600);
                    e
                },
                interrupt_entry(2, 10, 60),
            ],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        let result = sched.layouts_now(&CriteriaStore::default());
        // Both layouts appear, interrupt spread through rather than
        // clustered entirely at the start or end.
        assert!(result.contains(&1));
        assert!(result.contains(&2));
        let interrupt_count = result.iter().filter(|&&id| id == 2).count();
        assert_eq!(interrupt_count, 6);
        // Not clustered: shouldn't be e.g. [2,2,2,2,2,2,1,1,1,1,1,1] --
        // check that the first interrupt appears reasonably early, not
        // only at the very end.
        let first_interrupt_pos = result.iter().position(|&id| id == 2).unwrap();
        assert!(first_interrupt_pos < result.len() - 1,
                "interrupt should not be clustered only at the very end");
    }

    #[test]
    fn multiple_interrupts_each_satisfy_own_target() {
        let sched = Schedule {
            default: None,
            schedules: vec![
                {
                    let mut e = entry(1, 0, vec![]);
                    e.duration = Some(600);
                    e
                },
                interrupt_entry(2, 5, 60),  // target 180s -> 3 plays
                interrupt_entry(3, 5, 60),  // target 180s -> 3 plays
            ],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        let result = sched.layouts_now(&CriteriaStore::default());
        assert_eq!(result.iter().filter(|&&id| id == 2).count(), 3);
        assert_eq!(result.iter().filter(|&&id| id == 3).count(), 3);
    }

    #[test]
    fn no_normal_layout_falls_back_to_default_when_interrupts_present() {
        let sched = Schedule {
            default: Some(99),
            schedules: vec![interrupt_entry(2, 10, 60)],
            overlays: vec![], commands: vec![], actions: vec![],
        };
        let result = sched.layouts_now(&CriteriaStore::default());
        assert!(result.contains(&99));
        assert!(result.contains(&2));
    }
}


#[cfg(test)]
mod overlay_tests {
    use super::*;

    fn mk_overlay(layoutid: LayoutId, duration: i64, active: bool) -> OverlayEntry {
        let now = OffsetDateTime::now_local().unwrap();
        let (from, to) = if active {
            (now - time::Duration::hours(1), now + time::Duration::hours(1))
        } else {
            (now - time::Duration::hours(3), now - time::Duration::hours(2))
        };
        OverlayEntry { from, to, layoutid, duration, priority: 0, scheduleid: 1 }
    }

    #[test]
    fn no_overlays_means_empty_active_list() {
        let sched = Schedule { default: None, schedules: vec![], overlays: vec![], commands: vec![], actions: vec![] };
        assert!(sched.active_overlays().is_empty());
    }

    #[test]
    fn single_active_overlay_is_returned_with_its_duration() {
        let sched = Schedule {
            default: None, schedules: vec![],
            overlays: vec![mk_overlay(727, 91, true)], commands: vec![], actions: vec![],
        };
        assert_eq!(sched.active_overlays(), vec![(727, 91)]);
    }

    #[test]
    fn expired_overlay_is_not_active() {
        let sched = Schedule {
            default: None, schedules: vec![],
            overlays: vec![mk_overlay(727, 91, false)], commands: vec![], actions: vec![],
        };
        assert!(sched.active_overlays().is_empty());
    }

    #[test]
    fn multiple_simultaneously_active_overlays_all_returned() {
        let sched = Schedule {
            default: None, schedules: vec![],
            overlays: vec![mk_overlay(727, 91, true), mk_overlay(728, 30, true)], commands: vec![], actions: vec![],
        };
        let active = sched.active_overlays();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&(727, 91)));
        assert!(active.contains(&(728, 30)));
    }

    #[test]
    fn mixed_active_and_expired_only_active_returned() {
        let sched = Schedule {
            default: None, schedules: vec![],
            overlays: vec![mk_overlay(727, 91, true), mk_overlay(999, 30, false)], commands: vec![], actions: vec![],
        };
        assert_eq!(sched.active_overlays(), vec![(727, 91)]);
    }

    #[test]
    fn parses_real_overlays_xml_from_user() {
        // Exact structure from a real schedule.xml the user shared
        // (attribute names/values verbatim), confirming the parser
        // handles the real wire format correctly.
        let xml = r#"<schedule generated="2026-08-06 11:39:20" filterFrom="2026-08-06 11:00:00" filterTo="2026-08-08 11:00:00">
  <layout file="614" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="210" priority="0" syncEvent="0" shareOfVoice="0" duration="60" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="106" playCount="0" maxPlaysPerHour="0"/>
  <overlays>
    <overlay file="727" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="211" priority="0" duration="91" isGeoAware="0" geoLocation="" maxPlaysPerHour="0"/>
  </overlays>
  <default file="614" duration="60"/>
</schedule>"#;
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        let sched = Schedule::parse(&tree).unwrap();
        assert_eq!(sched.active_overlays(), vec![(727, 91)]);
        assert_eq!(sched.overlay_scheduleid_for(727), 211);
    }
}

#[cfg(test)]
mod scheduled_command_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn parses_real_scheduled_commands_xml_from_user() {
        // Exact structure from a real schedule.xml the user shared (an
        // hourly "TESTTOUCH" command over several days) -- confirms the
        // parser handles the real wire format correctly, including
        // alongside <layout>/<default>/<dependants> in the same document.
        let xml = r#"<schedule generated="2026-08-26 11:27:46" filterFrom="2026-08-26 11:00:00" filterTo="2026-08-28 11:00:00">
  <command date="2026-08-26 11:00:00" scheduleid="234" code="TESTTOUCH"/>
  <command date="2026-08-26 12:00:00" scheduleid="235" code="TESTTOUCH"/>
  <layout file="964" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="227" priority="0" syncEvent="0" shareOfVoice="0" duration="30" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="146" playCount="0" maxPlaysPerHour="0"/>
  <default file="913" duration="60"/>
</schedule>"#;
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        let sched = Schedule::parse(&tree).unwrap();
        assert_eq!(sched.commands.len(), 2);
        assert_eq!(sched.commands[0].scheduleid, 234);
        assert_eq!(sched.commands[0].code, "TESTTOUCH");
        assert_eq!(sched.commands[1].scheduleid, 235);
    }

    fn sched_with(date: OffsetDateTime, scheduleid: i64) -> Schedule {
        Schedule { default: None, schedules: vec![], overlays: vec![],
                   commands: vec![ScheduledCommand { date, scheduleid, code: "TESTTOUCH".into() }], actions: vec![] }
    }

    #[test]
    fn a_command_right_at_its_own_due_time_is_due() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = sched_with(now, 1);
        assert_eq!(sched.commands_due(now, &HashSet::new()), vec![(1, "TESTTOUCH".to_string())]);
    }

    #[test]
    fn a_command_within_the_10s_window_is_due() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = sched_with(now - time::Duration::seconds(9), 1);
        assert_eq!(sched.commands_due(now, &HashSet::new()).len(), 1);
    }

    #[test]
    fn a_command_past_the_10s_window_is_no_longer_due() {
        // Confirms the fix matches the reference client's own semantics
        // (see ScheduledCommand's own doc comment) -- a command missed
        // due to an outage/restart must NOT fire late, unlike a real
        // historical bug this exact check is modeled to avoid
        // (xibo/xibo#1327: commands re-firing/firing long after their
        // own scheduled time).
        let now = OffsetDateTime::now_local().unwrap();
        let sched = sched_with(now - time::Duration::seconds(11), 1);
        assert!(sched.commands_due(now, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_command_not_yet_due_is_not_due() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = sched_with(now + time::Duration::seconds(5), 1);
        assert!(sched.commands_due(now, &HashSet::new()).is_empty());
    }

    #[test]
    fn an_already_run_command_is_not_due_again() {
        // Confirms the other half of the same historical bug (xibo/
        // xibo#1327) -- a command that already fired must not fire
        // again just because it's still within the window on a
        // subsequent check.
        let now = OffsetDateTime::now_local().unwrap();
        let sched = sched_with(now, 1);
        let mut already_run = HashSet::new();
        already_run.insert(1);
        assert!(sched.commands_due(now, &already_run).is_empty());
    }

    #[test]
    fn only_the_not_yet_run_command_is_returned_among_several() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = Schedule {
            default: None, schedules: vec![], overlays: vec![],
            commands: vec![
                ScheduledCommand { date: now, scheduleid: 1, code: "A".into() },
                ScheduledCommand { date: now, scheduleid: 2, code: "B".into() },
            ],
            actions: vec![],
        };
        let mut already_run = HashSet::new();
        already_run.insert(1);
        assert_eq!(sched.commands_due(now, &already_run), vec![(2, "B".to_string())]);
    }
}

#[cfg(test)]
mod scheduled_action_tests {
    use super::*;

    #[test]
    fn parses_real_scheduled_actions_xml_from_user() {
        // Exact structure from a real schedule.xml the user shared,
        // alongside <layout>/<default>/<dependants> in the same
        // document -- confirms the parser handles the real wire format
        // correctly, including a real trigger code targeting a real
        // layout by its own layoutCode.
        let xml = r#"<schedule generated="2026-08-27 11:35:27" filterFrom="2026-08-27 11:00:00" filterTo="2026-08-29 11:00:00">
  <layout file="971" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="254" priority="0" syncEvent="0" shareOfVoice="0" duration="30" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="137" playCount="0" maxPlaysPerHour="0"/>
  <actions>
    <action fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="253" priority="0" duration="0" isGeoAware="0" geoLocation="" triggerCode="apri_home" actionType="navLayout" layoutCode="defaultxibomultimedia" commandCode=""/>
  </actions>
  <default file="982" duration="60"/>
</schedule>"#;
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        let sched = Schedule::parse(&tree).unwrap();
        assert_eq!(sched.actions.len(), 1);
        assert_eq!(sched.actions[0].scheduleid, 253);
        assert_eq!(sched.actions[0].duration, 0);
        assert_eq!(sched.actions[0].trigger_code, "apri_home");
        assert_eq!(sched.actions[0].target,
                   ActionTarget::Layout("defaultxibomultimedia".into()));
    }

    #[test]
    fn parses_a_command_type_action() {
        // Same element shape, actionType="command" instead -- confirms
        // both branches of the real "navLayout or run a Command"
        // manual-documented mechanism, not just the one real capture
        // happened to show.
        let xml = r#"<schedule generated="2026-08-27 11:35:27" filterFrom="2026-08-27 11:00:00" filterTo="2026-08-29 11:00:00">
  <actions>
    <action fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="260" priority="0" duration="0" isGeoAware="0" geoLocation="" triggerCode="run_touch" actionType="command" layoutCode="" commandCode="TESTTOUCH"/>
  </actions>
</schedule>"#;
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        let sched = Schedule::parse(&tree).unwrap();
        assert_eq!(sched.actions[0].target, ActionTarget::Command("TESTTOUCH".into()));
    }

    fn real_action(trigger_code: &str, duration: i64, target: ActionTarget) -> Schedule {
        let now = OffsetDateTime::now_local().unwrap();
        Schedule {
            default: None, schedules: vec![], overlays: vec![], commands: vec![],
            actions: vec![ScheduledAction {
                from: now - time::Duration::hours(1), to: now + time::Duration::hours(1),
                scheduleid: 1, duration, trigger_code: trigger_code.into(), target,
            }],
        }
    }

    #[test]
    fn a_matching_trigger_code_within_its_window_returns_the_action() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = real_action("apri_home", 30, ActionTarget::Layout("home".into()));
        assert_eq!(sched.action_for_trigger("apri_home", now),
                   Some((30, &ActionTarget::Layout("home".into()))));
    }

    #[test]
    fn a_nonmatching_trigger_code_returns_none() {
        let now = OffsetDateTime::now_local().unwrap();
        let sched = real_action("apri_home", 30, ActionTarget::Layout("home".into()));
        assert_eq!(sched.action_for_trigger("something_else", now), None);
    }

    #[test]
    fn a_trigger_code_outside_its_own_window_returns_none() {
        let now = OffsetDateTime::now_local().unwrap();
        let mut sched = real_action("apri_home", 30, ActionTarget::Layout("home".into()));
        // Window already expired.
        sched.actions[0].to = now - time::Duration::minutes(1);
        assert_eq!(sched.action_for_trigger("apri_home", now), None);
    }

    #[test]
    fn duration_zero_is_returned_verbatim_for_the_caller_to_interpret() {
        // schedule.rs itself doesn't interpret duration==0 specially --
        // that's mainloop.rs's own handle_trigger_code (timer vs. wait
        // for natural completion). Just confirms the real value (0)
        // round-trips correctly through parsing and lookup.
        let now = OffsetDateTime::now_local().unwrap();
        let sched = real_action("apri_home", 0, ActionTarget::Layout("home".into()));
        assert_eq!(sched.action_for_trigger("apri_home", now).map(|(d, _)| d), Some(0));
    }
}

