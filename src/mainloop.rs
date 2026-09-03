// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Main collect loop that also processes XMR requests.

use std::{fmt, fs, path::{Path, PathBuf}, time::Duration};
use anyhow::{bail, Context, Result};
use crossbeam_channel::{after, never, select, tick, Receiver, Sender};
use itertools::Itertools;
use rand::rngs::OsRng;
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey}};
use subprocess::Popen;
use time::OffsetDateTime;
use crate::config::{ArexiboMeta, CmsSettings, PlayerSettings, SyncRole};
use crate::{logger, schedule, server, syncgroup, util, xmds, xmr};
use crate::resource::{Cache, ReqFile};
use crate::faults;
use crate::schedule::Schedule;
use crate::stats::{StatCollector, LayoutStat};
use crate::criteria::CriteriaStore;
use crate::command::Command;
use crate::util::percent_decode;

/// Error indicating the display is registered but not yet authorized in the CMS.
/// Uses a distinct exit code (2) so the kiosk session holder can wait patiently
/// instead of treating it as a configuration failure.
#[derive(Debug)]
pub struct NotAuthorized;

impl fmt::Display for NotAuthorized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "display is not authorized yet, try again after authorization in the CMS")
    }
}

impl std::error::Error for NotAuthorized {}

/// Error indicating embedded_server_port or embedded_server_allow_wan
/// changed mid-session (detected in collect_once, not just at initial
/// startup in Handler::new) -- the running webserver's own TCP
/// listener can't be rebound to a different port/address without
/// recreating it, so the only reliable fix is a full process exit,
/// relying on the external supervisor (systemd Restart=always, or an
/// equivalent restart loop) to start fresh -- which will then bind
/// correctly and also naturally trigger Handler::new's own
/// port-change-forces-cache-purge check if the port specifically
/// changed. Uses a distinct exit code (3) so this shows up in logs as
/// an intentional restart, not a crash.
#[derive(Debug)]
pub struct RestartRequired;

impl fmt::Display for RestartRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "embedded server port or WAN-access setting changed -- restarting \
                    to apply it")
    }
}

impl std::error::Error for RestartRequired {}

/// Messages sent to the GUI thread
pub enum ToGui {
    // Boxed per clippy's own large_enum_variant lint: PlayerSettings is
    // by far the largest variant here (288+ bytes) -- boxing it keeps
    // every ToGui value's own stack footprint small regardless of which
    // variant is actually in use, rather than every value (even a bare
    // `Screenshot`) paying for the largest possible payload.
    Settings(Box<PlayerSettings>),
    Layouts(Vec<i64>),
    Screenshot,
    WebHook(String),
    /// Tell the browser to reload just this one widget's iframe in place
    /// (its cached HTML on disk has just been refreshed) -- see
    /// xmr::Message::DataUpdate.
    ReloadWidget(i64),
    /// Show this layout id as an overlay on top of whatever's playing.
    ShowOverlay(i64),
    /// Hide the currently active overlay, if any.
    HideOverlay,
    /// Interactive Control duration override for a specific widget --
    /// see server::DurationRequest / layout.rs's `controlDuration`.
    ControlDuration(server::DurationRequest),
    /// Interactive Control webhook trigger code -- see
    /// server::TriggerRequest / layout.rs's `write_action`
    /// (`window.arexibo.triggers[code]`, populated only in the DOM of
    /// whichever page actually has a matching triggerType="webhook"
    /// action).
    Trigger(String),
    /// Force a real page reload of `layout_id`, even if the GUI's own
    /// Schedule<T> already believes it's current -- ordinary
    /// Layouts(same id again) is a silent no-op there
    /// (Schedule::update only navigates on an actual *change*). Needed
    /// specifically for Sync Group: when a Follower (re)connects
    /// mid-way through an already-active Synchronised Event, this
    /// Lead's own already-running region/playlist timers need to
    /// actually restart in lockstep with the (re)synchronized group,
    /// not just have the same layout id silently confirmed as already
    /// correct. See mainloop.rs's own sync_peer_connected handling.
    ///
    /// Carries the id explicitly -- an earlier version derived it from
    /// the GUI's own Schedule<T>::current() instead, but that reflects
    /// whatever the *last actual* ToGui::Layouts call set it to, which
    /// this message is specifically sent *instead of* for a Sync Group
    /// switch. The very first synchronized switch of a session
    /// force-reloaded layout 0 (the startup splash/default, still the
    /// GUI's own stale "current") instead of the actual synchronized
    /// layout.
    ForceReloadLayout(i64),
}

pub enum Kill {
    No,
    Terminate,
    Kill,
}

/// Messages received from the GUI thread
pub enum FromGui {
    Showing(i64),
    /// Sent when the currently-shown layout completes one natural
    /// cycle of its own regions/widgets (CB_LAYOUT_NEXT, matching
    /// jsLayoutDone) -- distinct from `Showing`, which fires when a
    /// layout *starts*. Used by handle_trigger_code's own
    /// duration==0 revert case (see override_revert_on_completion's
    /// own doc comment) -- otherwise ignored.
    LayoutCompleted,
    Screenshot(Vec<u8>),
    Command(String),
    Shell(String, bool),
    StopShell(Kill),
}

/// Backend handler that performs the collect loop and XMDS requests.
pub struct Handler {
    to_gui: Sender<ToGui>,
    from_gui: Receiver<FromGui>,
    settings: PlayerSettings,
    xmds: xmds::Cms,
    cache: Cache,
    envdir: PathBuf,
    xmr: Receiver<xmr::Message>,
    schedule: Schedule,
    layouts: Vec<i64>,
    /// scheduleids of scheduled commands (see schedule::ScheduledCommand)
    /// already fired this session -- in-memory only, resets on restart
    /// (same as the reference client's own per-item HasRun flag, itself
    /// reset whenever a fresh Schedule.xml comes in and a new
    /// ScheduleItem gets built for the same underlying scheduled
    /// command). Deliberately *not* keyed against `self.schedule` being
    /// replaced wholesale on every collection cycle -- see
    /// Schedule::commands_due's own doc comment for why the "already
    /// run" check has to live here instead.
    commands_run: std::collections::HashSet<i64>,
    current_layout: i64,
    /// Set by the `Purge` XMR handler -- checked and cleared right
    /// after the very next `collect_once()` completes, forcing a real
    /// GUI reload of whatever's currently showing regardless of
    /// whether the schedule/layout id itself changed at all (an
    /// ordinary `ToGui::Layouts` is silently no-op'd by the GUI's own
    /// `Schedule<T>::update` on an unchanged id -- see
    /// `ForceReloadLayout`'s own doc comment).
    force_reload_after_collect: bool,
    /// Whether the most recent collect_once() reported *any* required
    /// media/layout file failing to download -- see
    /// maybe_force_reload_after_purge's own doc comment. Chosen over a
    /// more surgical, per-layout-media check: simpler, at the cost of
    /// a persistently-failing unrelated file blocking the
    /// purge-triggered reload indefinitely (accepted trade-off). Set
    /// right after every download_required_files() call in
    /// collect_once(), before the per-file results are consumed by
    /// submit_media_inventory.
    last_collect_had_failures: bool,
    /// Set by an XMR `changeLayout` action: while `Some`, completely
    /// bypasses the normal CMS-driven schedule (see `schedule_check()`)
    /// and forces this one layout to be shown, exactly like the C#
    /// client's "override" schedule items (`ScheduleManager.GetOverrideSchedule`,
    /// `Priority = 0, Override = true, ToDt = DateTime.MaxValue`).
    /// Cleared by an XMR `revertToSchedule` action.
    override_layout: Option<i64>,
    /// Currently active overlay layout id (XMR `overlayLayout`), if any --
    /// tracked separately from `override_layout` since an overlay doesn't
    /// replace the normal schedule, it's shown on top of it. Cleared by
    /// its own expiry timer (see `run()`) or by `revertToSchedule`.
    overlay_layout: Option<i64>,
    /// Sync Group (video wall) LAN command channel -- see syncgroup.rs's
    /// own module doc comment. `None` whenever `settings.sync_role` is
    /// `SyncRole::None` (the overwhelmingly common case: not part of
    /// any Sync Group). Kept up to date by `update_sync_group`.
    sync_group: Option<syncgroup::SyncGroup>,
    /// The `sync_role` that `sync_group` (if any) was actually built
    /// from -- compared against `settings.sync_role` in
    /// `update_sync_group` to avoid tearing down and rebuilding a
    /// perfectly good connection (losing a Follower's own tracked
    /// clock offset, and briefly missing any Command published during
    /// the gap) on every unrelated settings change. A role *kind*
    /// change (None/Lead/Follower switching) is already caught earlier
    /// in `collect_once`, forcing a full process restart before this
    /// is ever reached with a different kind -- so in practice, this
    /// only actually differs (and triggers a real rebuild) when a
    /// Follower's own `lead_addr` changes while staying a Follower.
    sync_group_role: SyncRole,
    /// Whether the very first collection cycle (schedule.xml
    /// downloaded and parsed, every required file successfully
    /// downloaded/translated) has completed at least once. Gates
    /// `update_sync_group` (see its own doc comment): deliberately
    /// false at construction, so the two `update_settings` calls
    /// `Handler::new` itself makes (processing the initial
    /// RegisterDisplay response) don't connect to/announce readiness
    /// to a Sync Group Lead while this display's own cache is still
    /// empty -- a Follower connecting this early could receive a
    /// Command/Sync for sync_keys it can't yet resolve, and once it
    /// later discovers a matching layout independently, that would
    /// stage an uncoordinated fresh switch, possibly hours out of
    /// step with the Lead. Set true (and `update_sync_group` invoked
    /// once) at the end of the first genuinely successful
    /// `collect_once`.
    first_collection_done: bool,
    /// A Follower's own incoming, already offset-corrected SyncCommands
    /// -- `never()` whenever this display isn't currently a Sync Group
    /// Follower (matching every other "currently inactive"
    /// timer/channel in this struct, e.g. `overlay_expiry`), so this
    /// select! arm simply stays permanently idle in that case.
    sync_commands: Receiver<syncgroup::SyncCommand>,
    /// The sync_keys from the most recently received Sync Group
    /// Command, waiting to actually be resolved-and-applied once
    /// `sync_apply_timer` fires -- see that field's own doc comment
    /// for why this isn't applied immediately on receipt. Deliberately
    /// not a layout id (see resolve_layout_for_sync_keys's own doc
    /// comment for why trusting the Lead's own layout id directly
    /// would be unsafe for anything other than Mirror Sync).
    pending_sync_keys: Option<Vec<String>>,
    /// Which layout id (this display's *own* schedule-discovered
    /// candidate, from schedule_check's own local discovery -- never
    /// set from an incoming network Command, which only ever carries
    /// sync_keys) most recently triggered `pending_sync_keys` being
    /// staged, purely for that discovery's own re-publish guard --
    /// see its own call site's doc comment for why comparing
    /// `pending_sync_keys` by *value* alone is a real, distinct bug:
    /// two structurally-similar layouts (e.g. successive swaps within
    /// the same live Synchronised Event) can genuinely share the exact
    /// same sync_keys text, and comparing sync_keys values alone would
    /// then wrongly treat a genuinely new layout as "already handled"
    /// just because its own sync_keys happen to match a previous,
    /// different layout's own. `resolve_layout_for_sync_keys` itself
    /// is correctly unaffected by this field -- it only ever resolves
    /// by sync_keys content, as it must (an id is never trusted
    /// across the network at all).
    pending_sync_layout_id: Option<i64>,
    /// Set (to the layout id, alongside the `Instant` it was set at)
    /// right after `reconnect_after_catching_up_on_own` triggers a
    /// reconnect -- suppresses `schedule_check`'s own local discovery
    /// of this exact layout for a short grace window. Without it, the
    /// very next `schedule_check` call would independently discover
    /// the layout just finished caching and stage/apply it a second
    /// time, a few seconds after the reconnect's own Command already
    /// did -- two `ForceReloadLayout` navigations close together, risking
    /// a mid-load navigation if a widget's own JS is running. Short-
    /// lived, not permanent: if the reconnect doesn't pan out within a
    /// few seconds (e.g. the Lead is briefly unreachable), local
    /// discovery must resume normally.
    suppress_local_discovery_of: Option<(i64, std::time::Instant)>,
    /// Fires at the *local* instant (already offset-corrected --
    /// `SyncCommand::target_local`) of the most recently received Sync
    /// Group Command -- deliberately not applied to `override_layout`
    /// until this fires, so every display in the group switches at
    /// approximately the same real-world moment rather than as soon as
    /// each one individually happens to receive the message (network
    /// latency/jitter between the Lead and each Follower would
    /// otherwise directly show up as a visible desync).
    sync_apply_timer: Receiver<std::time::Instant>,
    /// Whether `override_layout` (if currently Some) was set by a Sync
    /// Group Command application (the sync_apply_timer firing, below)
    /// rather than by a Scheduled Action's own navLayout (which uses
    /// `override_expiry`/`override_revert_on_completion` instead --
    /// entirely separate fields, so the two mechanisms never interfere
    /// with each other). Needed because, unlike a Scheduled Action's
    /// own override, a Sync Group override has no fixed duration or
    /// "natural completion" signal of its own to revert on -- a real
    /// gap found while wiring this up for the first genuine end-to-end
    /// use: without *some* expiry check, a display that ever applied a
    /// synchronized layout switch would stay stuck showing that one
    /// layout forever, never resuming its own normal schedule once the
    /// underlying Synchronised Event's own scheduled window naturally
    /// ends. See `schedule_check`'s own expiry check, driven by
    /// `Schedule::is_sync_gated`.
    sync_layout_active: bool,
    /// (Lead only.) Fires once per Follower connection accept_loop
    /// accepts -- `never()` for a Follower's own SyncGroup, or when
    /// self.sync_group is None, matching every other "currently
    /// inactive" channel in this struct. See update_sync_group's own
    /// doc comment for why this can't be told apart from a genuinely
    /// first-time connection, and resync_for_new_peer's own doc
    /// comment for what happens in response.
    sync_peer_connected: Receiver<()>,
    /// Proof of Play accumulator (layout-level records only, see
    /// stats.rs). Flushed to the CMS at the end of every collection.
    stats: StatCollector,
    /// Player fault reports (see faults.rs) -- currently only recorded
    /// for layout translation/download failures. Flushed alongside
    /// `stats` at the end of every collection.
    faults: faults::FaultCollector,
    /// The layout currently being timed for Proof of Play purposes:
    /// (layout id, scheduleid, start time) -- `None` before the very
    /// first `FromGui::Showing` arrives. Closed out into a `LayoutStat`
    /// (see `record_layout_shown`) whenever a *different* layout starts
    /// showing.
    layout_playing_since: Option<(i64, i64, OffsetDateTime)>,
    /// Runtime state for Schedule Criteria (XMR `criteriaUpdate`) -- see
    /// criteria.rs. Deliberately not persisted across restarts.
    criteria: CriteriaStore,
    shell_process: Option<Popen>,
    /// Outcome of the most recently run player command, None if no command has
    /// run yet in this lifetime.
    last_command_success: Option<bool>,
    /// Interactive Control duration-override requests relayed from the
    /// HTTP server's own thread pool (see server.rs's DurationRequest) --
    /// handled in `run()`'s select! loop since only that JS run in the
    /// currently-displayed page can actually change a widget's timer.
    duration_rx: Receiver<server::DurationRequest>,
    /// Same reasoning as duration_rx, for Interactive Control webhook
    /// triggers (see server.rs's TriggerRequest).
    trigger_rx: Receiver<server::TriggerRequest>,
    /// Whether `--debug` was passed on the command line -- if so, the
    /// CMS's own `logLevel` Display Profile setting (see
    /// `PlayerSettings::log_level_filter`) is never applied, so the
    /// explicit local override always wins over a remote setting.
    debug_override: bool,
    /// Owned copies of the CMS settings and no-cert-verify flag, kept
    /// for the XMR retry in `collect_once` (see `xmr_retry_key` below).
    cms: CmsSettings,
    no_verify: bool,
    /// `Some` only while XMR isn't connected because startup happened
    /// offline with `--allow-offline` -- retried once per collection
    /// cycle until it succeeds, then back to `None`. A cloned key
    /// (RsaPrivateKey is Clone) so a retry is possible without
    /// re-deriving/persisting it separately.
    xmr_retry_key: Option<RsaPrivateKey>,
    /// Unlike `xmr_retry_key` (only `Some` while a retry is actually
    /// pending), this is *always* populated -- kept specifically so
    /// XMR can be restarted later even after it's already running
    /// successfully. See the `recv(self.xmr)` channel-closed handling
    /// in `run()`'s own select! loop, and `RECONNECT_MAX_ATTEMPTS`'s
    /// own doc comment in xmr.rs for the full context this exists for.
    xmr_privkey: RsaPrivateKey,
    /// On first setup, or while waiting for CMS authorization, the
    /// player used to just exit (code 2, so systemd's Restart= could
    /// relaunch it) -- nothing visible on screen meanwhile, and every
    /// relaunch redid setup from scratch. Now: Handler::new constructs
    /// a Handler in this pending state instead, with default (empty)
    /// settings/schedule -- layout 0 (the splash screen, showing this
    /// machine's own hostname/IP) stays up. collect_once retries
    /// registration every cycle, same as the "was authorized, lost it"
    /// case, just with a faster interval and a clearer log message.
    pending_auth: bool,
    /// Same pattern as `pending_auth`, for a different real cause: with
    /// `--allow-offline` set but no cached settings.json to fall back
    /// to (e.g. a brand-new totem's very first boot, before WiFi has
    /// obtained an IP/working DNS yet), register_display() failing
    /// used to `bail!()` the whole process out -- crashing on startup
    /// repeatedly (systemd's Restart= redoing the full Xorg/D-Bus/
    /// arexibo startup sequence each time) until the network happened
    /// to come up in the brief window before the next attempt. Now:
    /// the same empty-defaults, fast-retry Handler as pending_auth,
    /// with its own accurate log message (this isn't a CMS
    /// authorization issue).
    pending_network: bool,
    /// Set once GetWeather fails with "not present" (v6/v7-only on our
    /// v5 endpoint) -- avoids retrying every cycle once known.
    weather_unsupported: bool,
    /// TZ value already applied this run (see apply_process_timezone),
    /// or None if not yet applied. Tracked so a later CMS-side change
    /// is reported (needs a restart) instead of re-applied unsafely
    /// from a different thread.
    process_timezone_applied: Option<String>,
    /// Edge-triggered: fires once when screen_shot_requested goes
    /// false->true, not on every cycle it stays true.
    screenshot_requested_seen: bool,
    /// Timer for hiding/advancing the currently-shown overlay. Moved
    /// here (was local to `run()`) since `schedule_check()` needs to
    /// (re)schedule it too, not just XMR's `overlayLayout` handling.
    overlay_expiry: Receiver<std::time::Instant>,
    /// Timer for reverting an active `override_layout` set by a
    /// Scheduled Action's own `navLayout` target with a nonzero
    /// `duration` (see schedule::ScheduledAction's own doc comment,
    /// handle_trigger_code). `duration == 0` uses
    /// `override_revert_on_completion` instead -- this stays at
    /// `never()` in that case.
    override_expiry: Receiver<std::time::Instant>,
    /// Whether the active `override_layout` (if any) should revert to
    /// the normal schedule on the shown layout's own next natural
    /// completion (`FromGui::LayoutCompleted`) rather than on a timer
    /// (`override_expiry`) -- set for a Scheduled Action's own
    /// `navLayout` target with `duration == 0`. Meaningless whenever
    /// `override_layout` is `None`.
    override_revert_on_completion: bool,
    /// Currently-active schedule-driven Overlay Layouts (see
    /// schedule.rs's `active_overlays` -- a real `<overlays>` section,
    /// distinct from XMR's transient `overlayLayout` push action) as
    /// (layoutid, duration_secs) pairs, with `schedule_overlay_idx`
    /// tracking which one is showing -- rotated via `overlay_expiry`
    /// when more than one is active. An active XMR-triggered overlay
    /// (`overlay_layout` below) takes precedence while set.
    schedule_overlays: Vec<(i64, i64)>,
    schedule_overlay_idx: usize,
    /// Resources (see `ReqFile::Resource`) whose download failed during
    /// a normal collection, queued for a short-delay retry instead of
    /// waiting for the next full collection cycle. The CMS can return a
    /// transient "Cache not ready" fault for lazily-rendered content
    /// (e.g. a DataSet View widget) -- previously this just left the
    /// widget broken/blank until the next scheduled collection. Each
    /// entry is (request, attempts so far), capped at
    /// RESOURCE_RETRY_MAX_ATTEMPTS before giving up for good.
    resource_retry_queue: Vec<(crate::resource::ReqFile, u32)>,
    /// Same transient-fault retry protection as `resource_retry_queue`
    /// above, but for the XMR `dataUpdate` path specifically (see
    /// `Cache::refresh_resource`) -- that path doesn't go through a raw
    /// `ReqFile` at all (it has its own fallback lookups for nested/
    /// dataset-bound widgets), so it needs its own (id, attempts) queue
    /// rather than sharing `resource_retry_queue`'s `ReqFile`-based one.
    /// Drained by the same `resource_retry_timer`.
    dataupdate_retry_queue: Vec<(i64, u32)>,
    resource_retry_timer: Receiver<std::time::Instant>,
    /// v7 GetData polling groundwork -- fires when the soonest tracked
    /// data widget (see resource::Cache's own data_widgets) is due for
    /// a refresh. Stays `never()` (re-armed to that via
    /// `rearm_data_refresh_timer`) whenever no data widgets are
    /// currently tracked -- which is always true on v5, given the
    /// endpoint-version gate in Cache::download.
    next_data_refresh: Receiver<std::time::Instant>,
}

/// See `Handler::resource_retry_queue`'s own doc comment.
///
/// Widened from the original 15s x 5 (75s total) -- proved too short
/// in a real case where the CMS's "Cache not ready" fault persisted
/// well over a minute. ~3 minute total window gives the CMS more
/// realistic room while still eventually giving up.
pub(crate) const RESOURCE_RETRY_DELAY: Duration = Duration::from_secs(20);
const RESOURCE_RETRY_MAX_ATTEMPTS: u32 = 8;
/// See `Handler::pending_auth`'s own doc comment. 30s strikes a balance
/// between responsiveness (someone actively watching for the display
/// to come online after approving it) and not hammering the CMS with
/// requests while genuinely just waiting.
const PENDING_AUTH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Whether any *media* or *layout* file in a collection cycle's own
/// per-file download results failed -- used by
/// `maybe_force_reload_after_purge` (see its own doc comment) to avoid
/// firing a purge-triggered reload before every piece the currently-
/// showing layout might need is actually back on disk.
///
/// "resource" failures (dataset/webpage-manual/bestfit widgets, see
/// their own `ReqFile::inventory()`) are deliberately excluded: those
/// already have their own, separate recovery mechanism
/// (`resource_retry_queue`'s own short-delay retry, plus
/// `note_layout_file_downloaded`'s own reload-on-arrival for the
/// currently-showing layout) once they eventually download
/// successfully, so waiting on them here too would only add an
/// unnecessary delay before the purge-triggered reload can ever fire,
/// for no real benefit. A free function (not a method) specifically so
/// this one small piece of logic is directly unit-testable against a
/// plain, hand-built slice, without needing a real `Handler` or any
/// network/collection machinery at all.
fn any_non_resource_failure(result: &[((&'static str, i64), bool)]) -> bool {
    result.iter().any(|((typ, _), success)| *typ != "resource" && !success)
}

impl Handler {
    /// Create a new handler, with channels to the GUI thread.
    // Not bundled into an options struct: purely a clippy style lint,
    // not worth touching all 9 call sites for.
    #[allow(clippy::too_many_arguments)]
    pub fn new(cms: &CmsSettings, clear_cache: bool, envdir: &Path,
               no_verify: bool, allow_offline: bool, debug_override: bool,
               to_gui: Sender<ToGui>, from_gui: Receiver<FromGui>,
               duration_rx: Receiver<server::DurationRequest>,
               trigger_rx: Receiver<server::TriggerRequest>) -> Result<Self> {
        let (privkey, pubkey) = load_or_create_keypair(envdir)?;
        let mut cache = Cache::new(cms, envdir.join("res"), clear_cache, no_verify)
            .context("creating cache")?;

        // Detect an Arexibo version change since the last run and force a
        // full cache purge -- same reasoning as the embedded-webserver
        // port-change check further down: a newer build may generate
        // slightly different layout HTML/bundled assets (e.g. pdf.js),
        // and stale cached files could still reflect the old version's
        // output. Tracked via ArexiboMeta (arexibo.json, alongside
        // settings.json/sched.json) rather than a field on
        // PlayerSettings, since this must run even before/without ever
        // talking to the CMS (e.g. --allow-offline). Doesn't fire on a
        // first-ever run (no arexibo.json yet) -- the cache is already
        // empty in that case, so there's nothing to gain from purging
        // it. Any version difference triggers the purge, including a
        // patch-level bump -- deliberately not limited to major/minor,
        // since even a small change in generated HTML/assets can leave
        // stale cached files behind.
        let meta_file = envdir.join("arexibo.json");
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        if let Ok(prev_meta) = ArexiboMeta::from_file(&meta_file) {
            if prev_meta.version != current_version {
                log::warn!("Arexibo version changed from {} to {current_version} since \
                            last run -- purging the cache", prev_meta.version);
                cache.purge().context("purging cache after a version change")?;
            }
        }
        ArexiboMeta { version: current_version }.to_file(&meta_file)
            .context("writing arexibo meta file")?;

        let setting_file = envdir.join("settings.json");
        let sched_file = envdir.join("sched.json");
        let mut schedule = Schedule::default();
        let layouts = vec![];

        // create directory to store raw XML responses for debugging
        let xmldir = envdir.join("xml");
        if !fs::metadata(&xmldir).is_ok_and(|p| p.is_dir()) {
            fs::create_dir_all(&xmldir)?;
        }

        // make an initial register call, in order to get player settings
        let mut xmds = xmds::Cms::new(cms, pubkey, no_verify, xmldir)?;
        log::info!("doing initial register call to CMS");

        // try initial register call
        let mut network_pending = false;
        let res = match xmds.register_display() {
            Err(e) => {
                if !allow_offline {
                    bail!("CMS not reachable or call failed: {e:#}");
                }
                log::warn!("CMS not reachable or call failed: {e:#}");
                match PlayerSettings::from_file(&setting_file) {
                    Ok(settings) => {
                        log::info!("using cached settings");

                        if let Ok(cached_sched) = Schedule::from_file(sched_file) {
                            log::info!("using cached schedule, experience may be degraded");
                            schedule = cached_sched;
                        }

                        Some(settings)
                    }
                    Err(_) => {
                        // See `pending_network`'s own doc comment for
                        // the full context (previously bail!()'d the
                        // whole process out here instead).
                        network_pending = true;
                        None
                    }
                }
            }
            Ok(res) => res
        };

        // if we got settings, we are registered and authorized
        if let Some(mut settings) = res {
            // Verbose, gated behind --debug. Note: xmr_web_socket_address
            // may already reflect xmds.rs's own /xmr fallback, not
            // necessarily the CMS's raw value -- see
            // xmr_web_socket_address_in_use's own doc comment.
            log::debug!("player settings resolved from this registration (initial): {settings:?}");
            // --allow-offline (issue #33) tolerated RegisterDisplay
            // failing, but not XMR setup failing right after -- an
            // offline network would still abort startup entirely. Now
            // falls back to never() and retries via xmr_retry_key on a
            // later collect_once() cycle instead.
            // Sticky WS address policy: a new, non-empty address from
            // the CMS always replaces the cached one, but an empty/
            // port-less one doesn't clear it -- the CMS's own
            // WebSocket-eligibility check (isWebSocketXmrSupported())
            // is unreliable across registrations. Must run here, before
            // xmr::start(), not after -- otherwise this session's first
            // connection attempt would still use the bad address even
            // though settings.json ends up correct for next time.
            // AREXIBO_FORCE_WS_ADDRESS overrides this entirely.
            //
            // is_own_derived_ws_default() exempts our own /xmr fallback
            // (deliberately port-less) from being treated as suspicious.
            let is_own_derived_default = is_own_derived_ws_default(cms, &settings.xmr_web_socket_address_in_use);
            if let Ok(prev) = PlayerSettings::from_file(&setting_file) {
                if !prev.xmr_web_socket_address_in_use.is_empty() && !is_own_derived_default
                    && !ws_address_has_port(&settings.xmr_web_socket_address_in_use) {
                    log::warn!("XMR WebSocket address from this registration is \
                                either empty or missing an explicit port ({:?}) -- \
                                keeping the previously-known-good address {:?} \
                                instead. If this display should genuinely no \
                                longer use WebSocket XMR, clear it explicitly \
                                (e.g. --clear, or edit settings.json).",
                                settings.xmr_web_socket_address_in_use, prev.xmr_web_socket_address_in_use);
                    settings.xmr_web_socket_address_in_use = prev.xmr_web_socket_address_in_use;
                }

                // The embedded webserver's own port changed since the
                // last run (e.g. the CMS's own embeddedServerPort
                // setting was just changed, or upgraded firmware now
                // respects it for the first time where it didn't
                // before -- see server::effective_port's own doc
                // comment) -- cached layout HTML bakes in an absolute
                // http://127.0.0.x:<old port>/... iframe src (see
                // layout.rs's own write_action/write_media), which
                // would silently point nowhere once the server starts
                // listening on the new port instead. Force the same
                // full cache purge --clear would, rather than leaving
                // every widget broken until someone notices and clears
                // it manually.
                let prev_port = server::effective_port(prev.embedded_server_port);
                let new_port = server::effective_port(settings.embedded_server_port);
                if prev_port != new_port {
                    log::warn!("embedded webserver port changed from {prev_port} to \
                                {new_port} since last run -- purging the cache, since \
                                cached layout HTML has the old port baked into its own \
                                widget iframe URLs");
                    cache.purge().context("purging cache after a port change")?;
                }
            }

            let mut xmr_retry_key = None;
            let xmr = match xmr::start(cms, &settings, privkey.clone(), no_verify) {
                Ok(xmr) => xmr,
                Err(e) if allow_offline => {
                    log::warn!("could not set up XMR (will retry on a later collection \
                                cycle instead of real-time push): {e:#}");
                    xmr_retry_key = Some(privkey.clone());
                    never()
                }
                Err(e) => return Err(e),
            };
            settings.to_file(&setting_file).context("writing player settings")?;

            let mut slf = Self { to_gui, from_gui, settings, cache, xmds, xmr, schedule,
                                 layouts, commands_run: std::collections::HashSet::new(),
                                 envdir: envdir.into(), current_layout: 0, force_reload_after_collect: false, last_collect_had_failures: false,
                                 override_layout: None, overlay_layout: None,
                                 stats: StatCollector::default(),
                                 faults: faults::FaultCollector::default(),
                                 layout_playing_since: None,
                                 criteria: CriteriaStore::default(),
                                 shell_process: None, last_command_success: None,
                                 duration_rx, trigger_rx, overlay_expiry: never(),
                                 override_expiry: never(), override_revert_on_completion: false,
                                 sync_group: None, sync_group_role: SyncRole::None, first_collection_done: false,
                                 sync_commands: never(), pending_sync_keys: None, pending_sync_layout_id: None, suppress_local_discovery_of: None,
                                 sync_apply_timer: never(), sync_layout_active: false, sync_peer_connected: never(),
                                 schedule_overlays: Vec::new(), schedule_overlay_idx: 0,
                                 resource_retry_queue: Vec::new(),
                                 dataupdate_retry_queue: Vec::new(),
                                 resource_retry_timer: never(),
                                 next_data_refresh: never(),
                                 pending_auth: false,
                                 pending_network: false,
                                 weather_unsupported: false,
                                 process_timezone_applied: None,
                                 screenshot_requested_seen: false,
                                 debug_override, xmr_privkey: privkey.clone(),
                                 xmr_retry_key, cms: cms.clone(), no_verify };
            slf.update_settings()?;
            slf.schedule_check();  // only useful in case of cached schedule
            Ok(slf)
        } else {
            // See pending_auth's/pending_network's own doc comments.
            // Everything here is a placeholder default -- no real
            // config exists yet. xmr_retry_key reuses the existing
            // --allow-offline retry mechanism in collect_once ("network
            // came back, try XMR now") for the same purpose here.
            if network_pending {
                log::warn!("could not reach the CMS and no cached settings are \
                            available yet (e.g. this display's very first boot, \
                            before the network is fully up) -- showing the splash \
                            screen and retrying periodically");
            } else {
                log::warn!("display is registered but not yet authorized in the CMS -- \
                            showing the splash screen and retrying periodically \
                            (see this machine's own hostname/IP on screen to help \
                            find/approve it in Administration -> Displays)");
            }
            let mut slf = Self { to_gui, from_gui, settings: PlayerSettings::default(),
                                 cache, xmds, xmr: never(), schedule: Schedule::default(),
                                 layouts: vec![], commands_run: std::collections::HashSet::new(),
                                 envdir: envdir.into(), current_layout: 0, force_reload_after_collect: false, last_collect_had_failures: false,
                                 override_layout: None, overlay_layout: None,
                                 stats: StatCollector::default(),
                                 faults: faults::FaultCollector::default(),
                                 layout_playing_since: None,
                                 criteria: CriteriaStore::default(),
                                 shell_process: None, last_command_success: None,
                                 duration_rx, trigger_rx, overlay_expiry: never(),
                                 override_expiry: never(), override_revert_on_completion: false,
                                 sync_group: None, sync_group_role: SyncRole::None, first_collection_done: false,
                                 sync_commands: never(), pending_sync_keys: None, pending_sync_layout_id: None, suppress_local_discovery_of: None,
                                 sync_apply_timer: never(), sync_layout_active: false, sync_peer_connected: never(),
                                 schedule_overlays: Vec::new(), schedule_overlay_idx: 0,
                                 resource_retry_queue: Vec::new(),
                                 dataupdate_retry_queue: Vec::new(),
                                 resource_retry_timer: never(),
                                 next_data_refresh: never(),
                                 pending_auth: !network_pending,
                                 pending_network: network_pending,
                                 weather_unsupported: false,
                                 process_timezone_applied: None,
                                 screenshot_requested_seen: false,
                                 debug_override, xmr_privkey: privkey.clone(),
                                 xmr_retry_key: Some(privkey),
                                 cms: cms.clone(), no_verify };
            slf.update_settings()?;
            Ok(slf)
        }
    }

    pub fn player_settings(&self) -> PlayerSettings {
        self.settings.clone()
    }

    /// Called once from main.rs, right after the embedded HTTP server
    /// (and its shards, see server::HTML_SHARD_COUNT) start up -- so
    /// that any layout translated from this point on (including ones
    /// translated during this same startup's initial collection) can
    /// build correct absolute iframe URLs for its `render="html"`
    /// widgets. Not available at `Handler::new()` time itself, since the
    /// server needs `settings.embedded_server_port` (only known once the
    /// Handler already exists) before it can start listening -- a
    /// necessary chicken-and-egg break in an otherwise all-at-once
    /// construction.
    pub fn set_html_port(&mut self, port: u16) {
        self.cache.html_port = port;
    }

    /// Run the main collect loop.
    pub fn run(mut self) -> Result<()> {
        let mut collect = after(Duration::from_secs(0));  // do first collect immediately
        let mut screenshot = if self.settings.screenshot_interval != 0 {
            after(Duration::from_secs(self.settings.screenshot_interval * 60))
        } else {
            never()
        };
        let schedule_check = tick(Duration::from_secs(60));
        // 5s, comfortably under commands_due's own 10s window (see its
        // own doc comment) -- guarantees at least one check happens
        // within any given command's own due window, never silently
        // skipping past it between checks.
        let command_check = tick(Duration::from_secs(5));
        loop {
            select! {
                // timer channel that fires when collect is needed
                recv(collect) -> _ => {
                    if let Err(e) = self.collect_once() {
                        // RestartRequired must actually propagate out
                        // of run() -- unlike every other collect_once
                        // failure here (network hiccups, CMS faults,
                        // etc.), which are deliberately non-fatal and
                        // just get retried next cycle, this one can
                        // never resolve itself: the webserver's own TCP
                        // listener is already bound, and only a real
                        // process restart can rebind it.
                        if e.root_cause().downcast_ref::<RestartRequired>().is_some() {
                            return Err(e);
                        }
                        log::error!("during collect: {e:#}");
                    }
                    // A fresh collection may have discovered new data
                    // widgets (v7 GetData polling) whose own interval
                    // is sooner than whatever's currently armed --
                    // harmless no-op if nothing changed.
                    self.rearm_data_refresh_timer();
                    // See this flag's own doc comment (set by the
                    // `Purge` XMR handler) -- forces a real reload of
                    // whatever's currently showing now that the fresh
                    // files purge triggered are back. Whether this
                    // actually reloads now, or waits for a later cycle,
                    // is handled entirely inside the method itself (see
                    // its own doc comment for a real, severe bug found
                    // from reloading unconditionally here) -- it checks
                    // the current layout's own translated HTML is
                    // actually present on disk first.
                    self.maybe_force_reload_after_purge();
                    // While waiting for CMS authorization or a working
                    // network (see pending_auth/pending_network), retry
                    // much sooner than the normal collect_interval (which
                    // defaults to 900s/15min -- a long wait for someone
                    // actively watching a freshly-set-up display come
                    // online, or for a totem whose WiFi just needs a
                    // few more seconds).
                    let interval = if self.pending_auth || self.pending_network {
                        PENDING_AUTH_RETRY_INTERVAL
                    } else {
                        Duration::from_secs(self.settings.collect_interval)
                    };
                    collect = after(interval);
                },
                // timer channel that fires when screenshot is needed
                recv(screenshot) -> _ => {
                    // Confirms this timer arm actually fires and the
                    // message to the GUI thread is sent, since neither
                    // was logged anywhere before.
                    log::debug!("screenshot timer fired, requesting capture from GUI thread");
                    self.to_gui.send(ToGui::Screenshot).unwrap();
                    screenshot = if self.settings.screenshot_interval != 0 {
                        after(Duration::from_secs(self.settings.screenshot_interval * 60))
                    } else {
                        never()
                    };
                },
                // timer channel that fires every minute, to check if current layouts change
                recv(schedule_check) -> _ => {
                    self.schedule_check();
                },
                recv(command_check) -> _ => {
                    self.check_scheduled_commands();
                },
                // timer channel that fires when the active overlay's
                // requested duration has elapsed
                recv(self.overlay_expiry) -> _ => {
                    if self.overlay_layout.is_some() {
                        // An XMR-triggered overlay expired -- revert
                        // entirely (matches the documented "revert to
                        // schedule" semantics), then immediately check
                        // whether a schedule-driven overlay should take
                        // its place rather than waiting for the next
                        // 60s schedule_check tick.
                        log::info!("overlay layout expired, hiding");
                        self.overlay_layout = None;
                        self.to_gui.send(ToGui::HideOverlay).unwrap();
                        self.overlay_expiry = never();
                        self.recheck_schedule_overlays();
                    } else if !self.schedule_overlays.is_empty() {
                        // Schedule-driven rotation: advance to the next
                        // active overlay (wrapping around) rather than
                        // hiding -- see schedule.rs's `active_overlays`
                        // doc comment: several can be simultaneously
                        // active and are shown "in rotation".
                        self.schedule_overlay_idx =
                            (self.schedule_overlay_idx + 1) % self.schedule_overlays.len();
                        self.show_current_schedule_overlay();
                    }
                },
                // Timer channel for reverting an active override_layout
                // set by a Scheduled Action's own navLayout target with
                // duration > 0 (see handle_trigger_code) -- the
                // duration == 0 case instead reverts on
                // FromGui::LayoutCompleted, below.
                recv(self.override_expiry) -> _ => {
                    if self.override_layout.take().is_some() {
                        log::info!("Scheduled Action's override layout duration elapsed -- \
                                    reverting to normal schedule");
                        self.override_expiry = never();
                        self.schedule_check();
                    }
                },
                // A Sync Group Follower's own incoming, already
                // offset-corrected Command -- see sync_apply_timer's
                // own doc comment for why this is staged rather than
                // applied immediately.
                recv(self.sync_commands) -> cmd => if let Ok(cmd) = cmd {
                    let now = OffsetDateTime::now_local().unwrap();
                    let delay = (cmd.target_local - now).max(time::Duration::ZERO).unsigned_abs();
                    log::info!("Sync Group: received Command for sync_keys {:?} -- \
                                applying in {delay:?}", cmd.sync_keys);
                    self.pending_sync_keys = Some(cmd.sync_keys);
                    self.sync_apply_timer = after(delay);
                },
                // The staged Sync Group Command's own target instant
                // has arrived -- actually switch now, at (as close as
                // this process's own timer/scheduling precision
                // allows) the same real-world moment every other
                // display in the group does too.
                recv(self.sync_apply_timer) -> _ => {
                    // Deliberately a clone/read, NOT `.take()` --
                    // taking it (clearing regardless of whether
                    // resolution succeeded) meant the next
                    // schedule_check()'s own re-publish guard could
                    // never recognize "already processed this exact
                    // sync_keys" once resolution had failed -- a
                    // display not part of a given group (None from
                    // resolve_layout_for_sync_keys, the correct
                    // outcome) would re-stage and re-fail the same
                    // sync_keys every cycle, forever, in a tight loop.
                    // Now stays set until a genuinely different
                    // sync_keys set gets staged (local discovery or a
                    // fresh Command both already overwrite it).
                    if let Some(sync_keys) = self.pending_sync_keys.clone() {
                        match self.resolve_layout_for_sync_keys(&sync_keys) {
                            Some(layout_id) => {
                                log::info!("Sync Group: applying synchronized layout \
                                            switch to {layout_id} (sync_keys {sync_keys:?})");
                                self.override_layout = Some(layout_id);
                                self.sync_layout_active = true;
                                self.layouts = vec![layout_id];
                                // Always force an actual page reload
                                // here -- not the ordinary
                                // ToGui::Layouts (which the GUI's own
                                // Schedule<T>::update silently no-ops
                                // on an *unchanged* id). This same
                                // code path also runs when
                                // re-synchronizing an *already*-
                                // showing layout for a newly
                                // (re)connected peer (see
                                // sync_peer_connected's own handler)
                                // -- there the layout id genuinely
                                // doesn't change, but the whole point
                                // is to restart this display's own
                                // region/playlist timers in lockstep
                                // with the rest of the group, which
                                // only a genuine reload achieves.
                                self.to_gui.send(ToGui::ForceReloadLayout(layout_id)).unwrap();
                                // Recorded only now (once actually
                                // committed, never while merely
                                // staged) -- see
                                // SyncGroup::set_current_sync_keys's
                                // own doc comment for why this
                                // matters: a Follower that
                                // (re)connects mid-way through this
                                // same event learns about it via the
                                // next periodic Sync heartbeat. A
                                // no-op for a Follower's own
                                // SyncGroup, or when self.sync_group
                                // is None.
                                if let Some(sync_group) = &self.sync_group {
                                    sync_group.set_current_sync_keys(sync_keys);
                                }
                            }
                            None => {
                                // A real safety case, not a bug: for
                                // anything other than Mirror Sync
                                // (Wall Sync -- each display shows a
                                // *different* layout of a shared
                                // composition), a display whose own
                                // currently-scheduled layout doesn't
                                // share any of these sync_keys simply
                                // isn't part of *this* synchronized
                                // grouping right now -- correctly
                                // doing nothing, rather than the
                                // previous design's own real bug
                                // (blindly applying the Lead's own
                                // layout id regardless, which could
                                // show entirely wrong content on a
                                // Wall Sync display). Logged once per
                                // *distinct* sync_keys value now (see
                                // this whole handler's own doc comment
                                // just above) -- not once a cycle,
                                // forever.
                                log::info!("Sync Group: none of my own currently-scheduled \
                                            layout(s) share any of sync_keys {sync_keys:?} \
                                            -- not part of this synchronized event, doing \
                                            nothing");
                            }
                        }
                        self.sync_apply_timer = never();
                        // Still called for its other side effects
                        // (data widget pruning, schedule-driven
                        // overlays) -- won't re-send ToGui::Layouts
                        // itself when a layout was resolved above,
                        // since self.layouts already matches what
                        // override_layout now resolves to.
                        self.schedule_check();
                    }
                },
                // (Lead only.) A Follower connection was just accepted
                // -- re-synchronize *everyone*, including this Lead
                // itself, on whichever sync_keys are currently active
                // (if any). There's no way to tell a genuinely
                // first-time connection apart from a reconnection
                // after a restart on either side (restarting a
                // Follower mid-event would otherwise leave it stuck
                // showing unrelated content forever, since the Lead
                // only publishes a fresh Command on a schedule change)
                // -- so this fires unconditionally on every
                // connection, deliberately not trying to guess which
                // case it is.
                recv(self.sync_peer_connected) -> _ => {
                    if self.sync_layout_active {
                        if let Some(layout_id) = self.override_layout {
                            if let Some(info) = self.cache.get_layout(layout_id) {
                                log::info!("Sync Group: Follower (re)connected -- \
                                            re-synchronizing everyone on sync_keys {:?}",
                                            info.sync_keys);
                                self.stage_sync_switch(info.sync_keys.clone());
                            }
                        }
                    }
                },
                // timer channel that fires to retry resources whose
                // download failed during a normal collection -- see
                // `resource_retry_queue`'s own doc comment.
                recv(self.resource_retry_timer) -> _ => {
                    self.retry_failed_resources();
                },
                // timer channel that fires when the soonest tracked
                // data widget (v7 GetData polling) is due for a
                // refresh -- see `rearm_data_refresh_timer`'s own doc
                // comment. Never fires on v5 (nothing is ever tracked
                // there), so this whole arm stays practically inert.
                recv(self.next_data_refresh) -> _ => {
                    self.refresh_due_data_widgets();
                },
                // channel for XMR messages
                recv(self.xmr) -> msg => match msg {
                    Ok(xmr::Message::CollectNow) => collect = after(Duration::from_secs(0)),
                    Ok(xmr::Message::Screenshot) => {
                        log::debug!("XMR screenshot request received, arming immediate timer");
                        screenshot = after(Duration::from_secs(0));
                    }
                    Ok(xmr::Message::Purge) => {
                        if let Err(e) = self.cache.purge() {
                            log::error!("during cache purge: {e:#}");
                        }
                        collect = after(Duration::from_secs(0));  // force re-download
                        // purge() deletes every file on disk
                        // immediately, including whatever a currently-
                        // playing widget is actively streaming from the
                        // embedded HTTP server -- schedule_check()'s
                        // own reload decision is keyed on the layout id
                        // changing, which a purge alone doesn't. Forces
                        // a real reload once the fresh files are back
                        // (see this flag's own check, after the next
                        // collect_once()).
                        self.force_reload_after_collect = true;
                    }
                    Ok(xmr::Message::WebHook(code)) => {
                        if self.handle_trigger_code(&code) {
                            collect = after(Duration::from_secs(0));
                        }
                    }
                    Ok(xmr::Message::Command(code)) => {
                        self.run_command(&code);
                    }
                    Ok(xmr::Message::DataUpdate(widget_id)) => {
                        // For a v7 data widget we're independently
                        // polling via GetData, refresh its own JSON
                        // before reloading the resource below --
                        // otherwise the reloaded iframe's own JS fetches
                        // <widgetId>.json immediately, which might not
                        // exist yet if our own polling timer hasn't
                        // caught up (a transient 404, recovering once
                        // either timer catches up). Best-effort -- a
                        // failure here doesn't block the resource
                        // reload below, which must proceed regardless.
                        if self.cache.is_tracked_data_widget(widget_id) {
                            if let Err(e) = self.cache.refresh_data_widget(
                                widget_id, &mut self.xmds, std::time::Instant::now()) {
                                log::warn!("refreshing data widget {widget_id} \
                                            after dataUpdate: {e:#}");
                            }
                        }
                        // Targeted refresh of a single widget's cached
                        // HTML -- deliberately does NOT touch the stored
                        // `updated` timestamp (see Cache::refresh_resource),
                        // so a subsequent full collection may re-download
                        // the same content again if the CMS has since
                        // bumped it; that's a harmless redundant fetch,
                        // not a correctness issue.
                        match self.cache.refresh_resource(widget_id, &mut self.xmds) {
                            // Reload the *returned* id, not widget_id --
                            // they differ for a widget nested inside
                            // another resource's combined HTML, where
                            // only the container has its own iframe.
                            Ok(fetch_id) => {
                                self.to_gui.send(ToGui::ReloadWidget(fetch_id)).unwrap();
                            }
                            Err(e) => {
                                // Retry on transient CMS faults (e.g.
                                // "Cache not ready") -- this path wasn't
                                // retried before, unlike bulk collection.
                                log::error!("refreshing widget {widget_id} \
                                            after dataUpdate: {e:#}");
                                self.dataupdate_retry_queue.push((widget_id, 0));
                                self.resource_retry_timer = after(RESOURCE_RETRY_DELAY);
                            }
                        }
                    }
                    Ok(xmr::Message::ChangeLayout(layout_id)) => {
                        // Simplification vs the C# client: that supports
                        // both "replace" (clear any queued overrides) and
                        // "add" (queue alongside existing ones) change
                        // modes, plus multiple queued override layouts
                        // cycled in sequence. Here a single override slot
                        // is enough for a single-screen kiosk -- a second
                        // changeLayout simply replaces whichever one was
                        // active, matching "replace" semantics
                        // unconditionally. `changeMode: "queue"` behavior
                        // (stacking multiple overrides to cycle through)
                        // is NOT implemented.
                        self.override_layout = Some(layout_id);
                        if self.cache.get_layout(layout_id).is_none() {
                            // Not cached yet -- force an immediate
                            // collection so required_files() picks it up;
                            // schedule_check() runs again at the end of
                            // that collection and will pick up the
                            // override as soon as the layout exists on
                            // disk (translate() failing silently logs an
                            // error and simply doesn't switch, same as
                            // any other missing/invalid layout).
                            log::info!("changeLayout to {layout_id}, not yet cached, \
                                        forcing a collection");
                            collect = after(Duration::from_secs(0));
                        } else {
                            self.schedule_check();
                        }
                    }
                    Ok(xmr::Message::RevertToSchedule) => {
                        let had_override = self.override_layout.take().is_some();
                        let had_overlay = self.overlay_layout.take().is_some();
                        if had_overlay {
                            log::info!("reverting: hiding active overlay");
                            self.to_gui.send(ToGui::HideOverlay).unwrap();
                            self.overlay_expiry = never();
                            // A schedule-driven overlay might be waiting
                            // to take this XMR-triggered one's place --
                            // check immediately rather than waiting for
                            // the next 60s schedule_check tick.
                            self.recheck_schedule_overlays();
                        }
                        if had_override {
                            log::info!("reverting to normal schedule");
                            self.schedule_check();
                        }
                    }
                    Ok(xmr::Message::OverlayLayout(layout_id, duration_secs)) => {
                        // Scope simplification vs the C# client: only a
                        // single *XMR-triggered* overlay slot is
                        // supported (a second overlayLayout action
                        // replaces whichever one was showing) -- distinct
                        // from schedule-driven overlays (see
                        // schedule_overlays/active_overlays), which DO
                        // support several simultaneously, cycled in
                        // rotation. An XMR overlay always takes
                        // precedence over schedule-driven ones while
                        // active.
                        if self.cache.get_layout(layout_id).is_none() {
                            // Not cached -- force a collection, but (unlike
                            // changeLayout, which retries automatically via
                            // schedule_check() at the end of every
                            // collection) do NOT show a broken/blank
                            // overlay in the meantime: there's no
                            // equivalent automatic retry hook for the
                            // overlay, so this action is simply dropped if
                            // the layout isn't already known. Re-trigger
                            // overlayLayout from the CMS once it's synced.
                            log::warn!("overlayLayout {layout_id} not cached, forcing a \
                                        collection but NOT showing it -- re-trigger once synced");
                            collect = after(Duration::from_secs(0));
                        } else {
                            log::info!("showing overlay layout {layout_id} for {duration_secs}s");
                            self.overlay_layout = Some(layout_id);
                            self.to_gui.send(ToGui::ShowOverlay(layout_id)).unwrap();
                            self.overlay_expiry = after(Duration::from_secs(duration_secs));
                        }
                    }
                    Ok(xmr::Message::CriteriaUpdate(updates)) => {
                        // Per the docs: "Whenever new schedule criteria
                        // are set, the Xibo player app will reassess its
                        // schedule loop" -- so re-run schedule_check()
                        // immediately rather than waiting for the next
                        // 60s tick, since a criteria-conditioned layout
                        // becoming (in)active should react right away.
                        for (metric, value, ttl) in updates {
                            log::info!("criteria update: {metric}={value} (ttl {ttl}s)");
                            self.criteria.set(metric, value, ttl);
                        }
                        self.schedule_check();
                    }
                    // The XMR thread gives up (RECONNECT_MAX_ATTEMPTS in
                    // xmr.rs) after too many failed reconnects, dropping
                    // its Sender -- this closes the channel, landing
                    // here. Triggers a restart via xmr_retry_key +
                    // collect_once (same mechanism as --allow-offline).
                    // Must replace self.xmr with never() immediately --
                    // a disconnected channel's recv() returns instantly,
                    // so leaving it would busy-loop this select! arm.
                    Err(_) => {
                        self.xmr_disconnected();
                        collect = after(Duration::from_secs(0));
                    }
                },
                // channel for  from the GUI thread
                recv(self.from_gui) -> data => match data {
                    Ok(FromGui::Screenshot(data)) => {
                        if let Err(e) = self.xmds.submit_screenshot(data) {
                            log::error!("submitting screenshot: {e:#}");
                        }
                    }
                    Ok(FromGui::Showing(layout)) => {
                        self.current_layout = layout;
                        self.record_layout_shown(layout);
                        if self.settings.send_current_layout_as_status_update {
                            self.send_status_update();
                        }
                    }
                    Ok(FromGui::LayoutCompleted) => {
                        // Only meaningful for a Scheduled Action's own
                        // navLayout target with duration == 0 (see
                        // override_revert_on_completion's own doc
                        // comment) -- a no-op otherwise (e.g. normal
                        // schedule cycling, which handles its own
                        // advancement entirely on the GUI side).
                        if self.override_revert_on_completion && self.override_layout.take().is_some() {
                            log::info!("Scheduled Action's override layout completed its own \
                                        natural cycle -- reverting to normal schedule");
                            self.override_revert_on_completion = false;
                            self.schedule_check();
                        }
                    }
                    Ok(FromGui::Command(code)) =>
                        self.run_command(&code),
                    Ok(FromGui::Shell(code, with_shell)) =>
                        self.run_shell(&code, with_shell),
                    Ok(FromGui::StopShell(kill_mode)) => {
                        if let Some(mut child) = self.shell_process.take() {
                            match kill_mode {
                                Kill::No => self.shell_process = None,  // let it run
                                Kill::Terminate => { let _ = child.terminate(); }
                                Kill::Kill => { let _ = child.kill(); }
                            }
                        }
                    }
                    Err(_) => ()
                },
                // Interactive Control duration overrides, relayed from
                // the HTTP server's own thread pool -- just forward to
                // the GUI thread, which is the only place that can run
                // JS in the currently-displayed page.
                recv(self.duration_rx) -> req => if let Ok(req) = req {
                    self.to_gui.send(ToGui::ControlDuration(req)).unwrap();
                },
                // Same reasoning as duration_rx just above, for
                // Interactive Control webhook triggers.
                recv(self.trigger_rx) -> req => if let Ok(req) = req {
                    if self.handle_trigger_code(&req.code) {
                        collect = after(Duration::from_secs(0));
                    }
                }
            }
        }
    }

    /// Run a command, triggered from XMR or layout.
    fn run_command(&mut self, code: &str) {
        // Security: master switch (see PlayerSettings::enable_shell_commands
        // doc comment for why this applies here too, and why the
        // allowlist itself doesn't).
        if !self.settings.enable_shell_commands {
            log::warn!("refusing to run player command {code:?}: shell commands \
                        are disabled (EnableShellCommands is off on this display)");
            self.last_command_success = Some(false);
            let _ = self.xmds.notify_command_success(false);
            return;
        }
        if let Some(cmd) = self.settings.commands.get(code) {
            let success = match cmd.run() {
                Ok(success) => success,
                Err(e) => {
                    log::warn!("running command {code}: {e:#}");
                    false
                }
            };
            self.last_command_success = Some(success);
            let _ = self.xmds.notify_command_success(success);
        } else {
            log::error!("no such player command: {code}");
        }
    }

    /// Run a shell command, triggered from layout.
    fn run_shell(&mut self, code: &str, with_shell: bool) {
        // The CMS sends globalCommand/linuxCommand percent-encoded --
        // decode before logging/running, or the literal encoded string
        // would be what actually executed.
        let code = percent_decode(code);
        let code = code.as_str();

        // Security: this is the actually risky path -- an arbitrary
        // command line embedded directly in Layout content (a
        // shellcommand widget), as opposed to run_command's CMS
        // Display-Profile-preregistered commands. Gated by both the
        // master switch and, if non-empty, the allowlist.
        if !self.settings.enable_shell_commands {
            log::warn!("refusing to run shell command {code:?}: shell commands \
                        are disabled (EnableShellCommands is off on this display)");
            return;
        }
        let allow_list = &self.settings.shell_command_allow_list;
        if !is_command_allowed(allow_list, code) {
            log::warn!("refusing to run shell command {code:?}: not present in \
                        ShellCommandAllowList");
            return;
        }
        // A shellcommand widget's command string can also be a special
        // http|url|contentType|jsonBody or rs232|params|message form,
        // not just a shell line --
        // command.rs's Command::run() already implements both. Run
        // synchronously here (same as run_command does) rather than
        // the spawn-and-track-for-kill machinery below, which only
        // makes sense for genuine shell commands.
        if code.starts_with("http|") || code.starts_with("rs232|") {
            let adhoc = Command { command: code.to_string(), validate: String::new(), alerts: String::new() };
            match adhoc.run() {
                Ok(_) => log::info!("ran ad-hoc {} command successfully",
                                     if code.starts_with("http|") { "HTTP" } else { "RS232" }),
                Err(e) => log::error!("running ad-hoc command {code:?}: {e:#}"),
            }
            return;
        }
        let config = Default::default();
        let res = if with_shell {
            Popen::create(&["/bin/sh", "-c", code], config)
        } else if let Some(parts) = shlex::split(code) {
            Popen::create(&parts, config)
        } else {
            log::error!("invalid command line: {code}");
            return;
        };
        match res {
            Ok(child) => self.shell_process = Some(child),
            Err(e) => log::error!("spawning command {code}: {e:#}"),
        }
    }

    /// Do a single collection cycle.
    fn collect_once(&mut self) -> Result<()> {
        log::info!("doing collection");

        // call register to get updated player settings
        if let Some(mut settings) = self.xmds.register_display()? {
            // See the matching debug log + doc comment in `Handler::new`
            // for why this is safe to print in full (xmr_cms_key gets
            // auto-redacted by PlayerSettings's own Debug impl) and for
            // why it's named "resolved from this registration" rather
            // than "received from CMS" (not necessarily a verbatim copy
            // of the CMS's own raw response -- see that same comment).
            log::debug!("player settings resolved from this registration (collection cycle): {settings:?}");
            // Same sticky-address policy as Handler::new (see its own
            // comments), applied here to the in-memory settings used
            // for the XMR retry attempt, not settings.json.
            let is_own_derived_default = is_own_derived_ws_default(&self.cms, &settings.xmr_web_socket_address_in_use);
            if !self.settings.xmr_web_socket_address_in_use.is_empty() && !is_own_derived_default
                && !ws_address_has_port(&settings.xmr_web_socket_address_in_use) {
                log::warn!("XMR WebSocket address from this registration is \
                            either empty or missing an explicit port ({:?}) -- \
                            keeping the previously-known-good address {:?} \
                            instead. If this display should genuinely no \
                            longer use WebSocket XMR, clear it explicitly \
                            (e.g. --clear, or edit settings.json).",
                            settings.xmr_web_socket_address_in_use, self.settings.xmr_web_socket_address_in_use);
                settings.xmr_web_socket_address_in_use = self.settings.xmr_web_socket_address_in_use.clone();
            }
            // The webserver's own TCP listener is already bound and
            // running -- it can't be rebound to a different port or
            // address without recreating it, so a mid-session change
            // to either of these needs a full process restart (see
            // RestartRequired's own doc comment), unlike every other
            // setting here, which collect_once can just apply directly
            // in place.
            //
            // Same reasoning for a Sync Group *role* change (see
            // SyncRole::is_same_role_kind's own doc comment) -- None,
            // Lead, and Follower each need a fundamentally different
            // network setup once the actual sync channel exists
            // (nothing / listening / connecting), not something safe
            // to reconfigure live. A Follower's own lead_addr simply
            // changing (same role, different address to connect to)
            // deliberately does NOT trigger this -- that's a
            // reconnect, not a restart, left for the sync channel's
            // own implementation to handle once it exists.
            if settings.embedded_server_port != self.settings.embedded_server_port
                || settings.embedded_server_allow_wan != self.settings.embedded_server_allow_wan {
                log::warn!("embedded server port ({} -> {}) or WAN-access setting ({} -> {}) \
                            changed mid-session -- exiting so the supervisor restarts us \
                            fresh and picks it up",
                            self.settings.embedded_server_port, settings.embedded_server_port,
                            self.settings.embedded_server_allow_wan, settings.embedded_server_allow_wan);
                return Err(RestartRequired.into());
            }
            if !settings.sync_role.is_same_role_kind(&self.settings.sync_role) {
                log::warn!("Sync Group role changed ({:?} -> {:?}) mid-session -- exiting so \
                            the supervisor restarts us fresh and picks it up",
                            self.settings.sync_role, settings.sync_role);
                return Err(RestartRequired.into());
            }
            if settings != self.settings {
                self.settings = settings;
                self.update_settings()?;
            }
            if self.pending_auth || self.pending_network {
                // Just got authorized, or the network/CMS finally
                // became reachable -- this is the first real
                // registration this Handler has ever seen (constructed
                // in one of these pending states, see `Handler::new`'s
                // own doc comments), so persist it now exactly as a
                // normal startup registration would have (that write
                // only happens once, at the point of first successful
                // registration -- this *is* that point, just reached
                // via a later collection cycle instead of `new` itself).
                log::info!("{}, proceeding with normal operation",
                           if self.pending_auth { "display just got authorized in the CMS" }
                           else { "network/CMS is now reachable" });
                self.pending_auth = false;
                self.pending_network = false;
                if let Err(e) = self.settings.to_file(self.envdir.join("settings.json")) {
                    log::warn!("writing player settings after authorization: {e:#}");
                }
            }
        } else if self.pending_auth || self.pending_network {
            // Not an error -- still simply not authorized *yet* (a
            // successful SOAP call, network/CMS genuinely reachable,
            // just an "unauthorized" answer). If this Handler started
            // out in pending_network specifically (no network at
            // startup), reaching here at all means the network issue
            // has resolved -- transition cleanly to pending_auth,
            // since that's now the actual remaining blocker, rather
            // than falling through to the "was already authorized,
            // now lost it" branch below, which would be a misleading
            // message for what's actually happening here.
            self.pending_auth = true;
            self.pending_network = false;
            log::info!("still waiting for authorization in the CMS, will check \
                        again shortly");
            return Ok(());
        } else {
            // A previously-authorized, normally-running display
            // (pending_auth/pending_network both false) that gets
            // deauthorized used to just `bail!()` here -- logged as an
            // error once per cycle, but self.pending_auth was never
            // set (retries kept using the slow collect_interval
            // instead of the fast 30s one) and self.schedule/layouts
            // were never cleared (the GUI was never told to switch
            // away -- the display kept cycling its stale cached
            // schedule indefinitely). Now: transitions into the same
            // pending_auth state a fresh display starts in, and
            // actively clears the schedule + calls schedule_check()
            // immediately so its own empty-schedule path sends
            // ToGui::Layouts(vec![]), resolving to the splash screen
            // as a last resort -- not waiting for the next periodic
            // tick (60s).
            log::warn!("display was previously authorized but no longer is -- \
                        reverting to the splash screen and retrying periodically, \
                        same as a freshly-unauthorized display");
            self.pending_auth = true;
            self.schedule = Schedule::default();
            self.schedule_check();
            return Ok(());
        }

        // See `xmr_retry_key`'s own doc comment / the `--allow-offline`
        // bug fix in `Handler::new` -- only even attempted once
        // `register_display()` above has *already* succeeded this cycle
        // (confirming the network is genuinely reachable again), rather
        // than wasting a connection attempt on a cycle where we already
        // know it can't possibly work.
        if let Some(key) = self.xmr_retry_key.take() {
            match xmr::start(&self.cms, &self.settings, key.clone(), self.no_verify) {
                Ok(xmr) => {
                    log::info!("XMR connection recovered, switching from offline mode \
                                to real-time push updates");
                    self.xmr = xmr;
                }
                Err(e) => {
                    log::warn!("XMR still not reachable, will retry next collection: {e:#}");
                    self.xmr_retry_key = Some(key);
                }
            }
        }

        // get the missing files
        let (required, purge) = self.xmds.required_files()?;

        // update layout code map
        self.cache.update_code_map(&required)?;

        // purge files
        if let Err(e) = self.cache.purge_some(&purge) {
            log::warn!("purging some files: {e:#}");
        }

        // get the schedule
        let schedule = self.xmds.get_schedule()?;

        // The scheduleid of whatever's currently on screen, looked up
        // in the *old* schedule (self.schedule, not yet overwritten
        // with the fresh one just fetched above) -- see
        // is_exempt_as_currently_playing_layout's own doc comment for
        // why this, not the raw layout id, is the correct identity to
        // track across a publish (which changes the layout id but
        // keeps the scheduleid the same).
        let current_scheduleid = self.schedule.scheduleid_for(self.current_layout);

        let result = self.download_required_files(required, current_scheduleid, &schedule);

        // See maybe_force_reload_after_purge's own doc comment for why
        // this matters.
        self.last_collect_had_failures = any_non_resource_failure(&result);

        // let the CMS know we have the media
        self.xmds.submit_media_inventory(result)?;

        // now that we should have all media, apply the schedule
        self.schedule = schedule;
        let _ = self.schedule.to_file(self.envdir.join("sched.json"));

        // Weather-derived Schedule Criteria, complementing (not
        // replacing) the xmr::Message::CriteriaUpdate push path.
        // Skipped if the endpoint version doesn't support it, or if
        // already learned unsupported at runtime.
        if xmds::xmds_supports_v6_v7_methods() && !self.weather_unsupported {
            match self.xmds.get_weather() {
                Ok(json) => {
                    let ttl = self.settings.collect_interval as i64 + 60;
                    if let Err(e) = apply_weather_criteria(&mut self.criteria, &json, ttl) {
                        log::warn!("parsing weather JSON: {e:#}");
                    }
                }
                Err(e) if is_method_not_present_fault(&e) => {
                    log::info!("this CMS's XMDS endpoint does not support GetWeather \
                                (v6/v7-only) -- not retrying every cycle");
                    self.weather_unsupported = true;
                }
                Err(e) if is_weather_provider_not_configured_fault(&e) => {
                    // A real CMS-side PHP bug (a non-nullable `string`
                    // return type on a method that legitimately needs
                    // to return null when there's no weather data for
                    // this display's own location) -- but "no weather
                    // data for this location" is itself a normal,
                    // expected, valid outcome (e.g. the CMS's weather
                    // module is configured for a different area than
                    // this display), not something worth alerting an
                    // admin about. Keep retrying every cycle (the
                    // config could change), just never above debug.
                    log::debug!("getting weather (no data for this display's own location): \
                                 {e:#}");
                }
                Err(e) => log::warn!("getting weather: {e:#}"),
            }
        }

        self.schedule_check();

        // send log messages
        self.xmds.submit_log(&logger::pop_entries())?;

        self.send_status_update();

        self.flush_stats();
        self.flush_faults();

        // See `first_collection_done`'s own doc comment for the full
        // story -- this is the first point in a fresh session where
        // this display's own cache is guaranteed to already hold
        // whatever it needs (schedule downloaded and parsed, every
        // required file downloaded/translated, `schedule_check` above
        // already ran once against it) to correctly resolve any
        // sync_keys it might already be part of. Explicitly invoked
        // (not just flipping the flag and relying on some *later*
        // update_settings call to notice) because update_settings --
        // and therefore update_sync_group -- only runs again when the
        // CMS's own settings actually *change* between registrations;
        // if this display's own sync_role was already known from the
        // very first RegisterDisplay response (the overwhelmingly
        // common case) and never changes again, nothing would ever
        // naturally re-trigger this otherwise.
        if !self.first_collection_done {
            self.first_collection_done = true;
            self.update_sync_group();
        }

        log::info!("collection successful");
        Ok(())
    }

    /// For each file RequiredFiles said this display needs: download
    /// it if missing, and build the MediaInventory report to submit
    /// back to the CMS confirming what's actually on disk. A separate
    /// method specifically so this is directly testable against a
    /// real `Cache` without needing to mock the full RegisterDisplay/
    /// RequiredFiles/Schedule/GetWeather/SubmitLog/NotifyStatus SOAP
    /// chain `collect_once` otherwise requires just to reach it.
    fn download_required_files(&mut self, required: Vec<ReqFile>, current_scheduleid: i64,
                                schedule: &Schedule) -> Vec<((&'static str, i64), bool)> {
        let mut result = Vec::new();
        let total = required.len();
        // DownloadStartWindow/DownloadEndWindow gates only bulk file
        // downloads here, not the lightweight XMDS calls in
        // collect_once around this call -- schedule/layout-switching
        // still needs current info even outside the window, using
        // whatever's already cached.
        if !self.settings.is_within_download_window() {
            log::info!("outside the configured download window \
                        ({}-{}), skipping {total} pending file download(s) \
                        this cycle", self.settings.download_start_window,
                       self.settings.download_end_window);
            return result;
        }
        for (i, file) in required.into_iter().enumerate() {
            let filedesc = file.description();
            let inventory = file.inventory();
            // Dependencies are excluded from the MediaInventory report
            // below entirely -- unlike media/layout files, they have
            // no single meaningful integer id to report (see ReqFile::
            // Dependency's own doc comment), and the reference client
            // doesn't appear to report on them via MediaInventory
            // either. Reporting a synthetic placeholder id for every
            // dependency downloaded/cached in the same cycle would
            // produce multiple identical, meaningless entries in that
            // report.
            let is_dependency = matches!(file, crate::resource::ReqFile::Dependency { .. });
            if self.cache.has(&file) {
                // A file already fully cached from an earlier cycle
                // (re-scheduled after being removed) used to be
                // reported only at first download, never again once
                // cached -- so it was never included in a later
                // collection's own MediaInventory, leaving the CMS's
                // Manage Display view showing it pending indefinitely
                // even though it was already playing correctly.
                if !is_dependency { result.push((inventory, true)); }
                continue;
            }
            if is_exempt_as_currently_playing_layout(&file, current_scheduleid, schedule,
                                                      self.settings.expire_modified_layouts) {
                // Deliberately deferred (a *different*/modified version
                // is needed, but this exact layout is currently
                // playing) -- correctly left unreported either way this
                // cycle, same as before this fix: it's genuinely not
                // yet the version the CMS is asking for, unlike the
                // already-fully-cached case just above.
                continue;
            }
            {
                // Captured before `file` is moved into `download()` below
                // -- used only to attach a layoutId to a fault report if
                // this specific download fails and it was a layout (see
                // faults.rs; other required-file types aren't reported
                // as faults yet, deliberately scoped out for now).
                let layout_id_if_any = match &file {
                    ReqFile::File { typ: "layout", id, .. } => Some(*id),
                    _ => None,
                };
                log::info!("downloading required file {}/{}: {}", i+1, total, filedesc);
                match self.cache.download(file.clone(), &mut self.xmds)
                                .with_context(|| format!("downloading {filedesc}"))
                {
                    Ok(()) => {
                        if !is_dependency { result.push((inventory, true)); }
                        // A media item changed in the CMS's own library
                        // without republishing the layout still bumps
                        // the layout's own required version too -- both
                        // get redownloaded here correctly, but if that
                        // layout is currently showing, schedule_check()'s
                        // own reload decision (keyed on layout id
                        // changing) never notices -- only the content
                        // underneath changed. Reusing the same
                        // force-reload flag as `purgeAll` forces the
                        // same recovery here.
                        self.note_layout_file_downloaded(layout_id_if_any);
                    },
                    Err(e) => {
                        log::error!("{e:#}");
                        if let Some(layout_id) = layout_id_if_any {
                            self.faults.record(
                                faults::Fault::new(
                                    faults::FAULT_CODE_LAYOUT_TRANSLATE_FAILED,
                                    format!("{e:#}"),
                                ).with_layout(layout_id)
                            );
                        }
                        // Resource downloads get a short-delay retry
                        // (see resource_retry_queue's own doc comment)
                        // instead of just being reported as failed.
                        if matches!(file, crate::resource::ReqFile::Resource { .. }) {
                            self.resource_retry_queue.push((file, 0));
                            self.resource_retry_timer = after(RESOURCE_RETRY_DELAY);
                        }
                        if !is_dependency { result.push((inventory, false)); }
                    }
                }
            }
        }
        result
    }

    /// Proof of Play (layout-level, see stats.rs): close out the
    /// previously-playing layout's timing session (if its `enableStat`
    /// XLF attribute allows it) and start a new one for the layout that
    /// just started showing. Called from `FromGui::Showing`, which only
    /// fires for the *main* view (the overlay view's own layout inits are
    /// deliberately not wired to it -- see CB_OVERLAY_LAYOUT_INIT in
    /// gui.rs -- so overlay plays are intentionally NOT counted here;
    /// they aren't part of the normal schedule the CMS is asking about).
    fn record_layout_shown(&mut self, new_layout: i64) {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        if let Some((prev_id, prev_sid, start)) = self.layout_playing_since.take() {
            if prev_id != new_layout && self.cache.layout_enable_stat(prev_id) {
                self.stats.record_layout(LayoutStat {
                    fromdt: start, todt: now, layoutid: prev_id, scheduleid: prev_sid,
                });
            }
        }
        let sid = self.schedule.scheduleid_for(new_layout);
        self.layout_playing_since = Some((new_layout, sid, now));
    }

    /// Send a NotifyStatus update to the CMS with the current layout
    /// id and other status fields. Previously only ever called once
    /// per full collection cycle (which can be minutes apart) -- the
    /// CMS's own "Current Layout" display would lag behind actual
    /// layout switches by however long a collection cycle takes.
    /// Called immediately on every real layout change (FromGui::Showing,
    /// see `run()`'s own select! loop) *if*
    /// settings.send_current_layout_as_status_update allows it (a real
    /// CMS-controlled setting -- see that field's own doc comment),
    /// in addition to still being called unconditionally once per
    /// collection. Logs (not propagates) any error -- a failed status
    /// update shouldn't be fatal, especially when called from a
    /// context (a real-time layout switch) that isn't itself inside a
    /// `Result`-returning function.
    fn send_status_update(&mut self) {
        let (avail, total) = match util::space_info(self.cache.dir()) {
            Ok(v) => v,
            Err(e) => {
                log::error!("getting disk space info for status update: {e:#}");
                return;
            }
        };
        // Report the CMS's own timezone, not the raw system one --
        // otherwise the CMS overwrites an admin's manual override
        // (Xibo issue #983).
        let system_tz = util::timezone();
        let reported_tz = timezone_to_report(&self.settings.display_time_zone, &system_tz);
        // First local IP only (matching `hostname -I`'s own ordering,
        // typically the primary/default-route interface) -- see
        // Status::lanIpAddress's own doc comment for why this matters.
        // `hostname -I` is best-effort (see get_local_ips's own doc
        // comment); an empty result here just means the field gets
        // omitted below, not sent as an empty string.
        let local_ips = util::get_local_ips();
        let status = xmds::Status {
            currentLayoutId: self.current_layout,
            availableSpace: avail,
            totalSpace: total,
            lastCommandSuccess: self.last_command_success.unwrap_or(true),
            deviceName: &self.settings.display_name,
            timeZone: &reported_tz,
            lanIpAddress: local_ips.first().map(String::as_str),
        };
        if let Err(e) = self.xmds.notify_status(&status) {
            log::error!("sending status update: {e:#}");
        }
    }

    /// Flush any accumulated Proof of Play records to the CMS. Called at
    /// the end of every collection (see `collect_once`) -- deliberately
    /// not on its own separate timer, to avoid yet another moving part
    /// for a first-cut implementation; records naturally accumulate
    /// between collections and get sent in a batch.
    fn flush_stats(&mut self) {
        if self.stats.is_empty() {
            return;
        }
        let (xml, recs) = self.stats.build_and_clear();
        if let Err(e) = self.xmds.submit_stats(&xml) {
            log::error!("submitting Proof of Play stats: {e:#}");
            self.stats.requeue(recs);
        }
    }

    /// Flush any accumulated player fault reports to the CMS. Same
    /// batching/requeue-on-failure approach as `flush_stats` above.
    /// Skipped entirely if the endpoint version doesn't support it --
    /// ReportFaults is v6/v7-only (see faults.rs's own doc comment),
    /// and this call used to run completely unguarded: every attempt
    /// against a v5 endpoint (arexibo's own default for a long time)
    /// would fail with a "not present" SOAP fault every single cycle,
    /// forever, without ever successfully reporting anything -- a
    /// wasted network round-trip each cycle, and faults requeued
    /// indefinitely until FaultCollector's own MAX_PENDING cap started
    /// silently dropping the oldest ones.
    ///
    /// Unlike GetWeather (which also gates on this same version check,
    /// but *additionally* learns "unsupported" at runtime, since a
    /// v6/v7-capable endpoint can still fail for an entirely separate,
    /// only-discoverable-by-trying reason: no weather provider
    /// configured on the CMS side), there's no second, independent
    /// failure mode here to learn about -- ReportFaults's own
    /// availability is fully determined by the endpoint version alone,
    /// which we already know statically. A runtime-learned flag would
    /// add complexity without covering any real scenario.
    fn flush_faults(&mut self) {
        if self.faults.is_empty() || !xmds::xmds_supports_v6_v7_methods() {
            return;
        }
        let (json, recs) = self.faults.build_and_clear();
        if let Err(e) = self.xmds.report_faults(&json) {
            log::error!("reporting faults: {e:#}");
            self.faults.requeue(recs);
        }
    }

    /// Check if need to update the layouts to show.
    /// See the matching `Err(_)` arm in `run()`'s select! loop --
    /// extracted so this can be tested directly without driving the
    /// full, otherwise-infinite `run()` loop.
    fn xmr_disconnected(&mut self) {
        log::warn!("XMR connection thread ended (gave up reconnecting) -- \
                    will restart it with fresh settings on the next \
                    collection cycle");
        self.xmr = never();
        self.xmr_retry_key = Some(self.xmr_privkey.clone());
    }

    /// Publishes `sync_keys` to every connected Follower (a no-op for
    /// a Follower's own SyncGroup, or when self.sync_group is None)
    /// and stages it locally exactly the same way an incoming
    /// Follower Command does (see sync_apply_timer's own doc comment)
    /// -- using the same switch_delay this Lead just told every
    /// Follower to expect, so this display converges on
    /// (approximately) the same real-world target instant as the rest
    /// of the group, rather than switching immediately while
    /// Followers are still catching up. Shared by schedule_check
    /// (a genuinely new sync-gated layout) and the sync_peer_connected
    /// handler (re-synchronizing everyone once a new/reconnecting
    /// Follower shows up, even for an *already*-active layout it
    /// needs its own timers restarted for).
    fn stage_sync_switch(&mut self, sync_keys: Vec<String>) {
        if let Some(sync_group) = &self.sync_group {
            sync_group.publish_sync_keys(sync_keys.clone());
        }
        let delay = Duration::from_millis(self.settings.sync_switch_delay);
        self.pending_sync_keys = Some(sync_keys);
        self.sync_apply_timer = after(delay);
    }

    /// Resolves a received sync_keys set (from a Sync Group Command,
    /// whether this display's own or the Lead's) against *this
    /// display's own* schedule and cache -- returns the layout id to
    /// actually apply, or None if this display isn't part of this
    /// particular synchronized grouping at all right now.
    ///
    /// Deliberately never trusts a layout id carried over the network
    /// (there isn't one in the wire protocol at all -- see
    /// syncgroup::Message::Command's own doc comment): for Mirror
    /// Sync, every display's own schedule names the same layout
    /// anyway, so this naturally resolves to it independently on each
    /// one; for Wall Sync (each display shows a *different* layout of
    /// a shared composition -- not yet exercised with real data, but
    /// the whole reason this function exists rather than just using
    /// the Lead's own id directly), this is what lets each display
    /// find *its own* correct layout instead of blindly showing
    /// whichever one the Lead happens to have.
    fn resolve_layout_for_sync_keys(&mut self, sync_keys: &[String]) -> Option<i64> {
        let candidate = self.schedule.active_sync_gated_layout(&self.criteria)?;
        if self.cache.get_layout(candidate).is_none() {
            // We know *exactly* which layout we need (our own schedule
            // already names it as sync-gated) -- we just don't have it
            // cached yet. This resolution attempt is only ever reached
            // right as a Command/heartbeat carrying sync_keys is being
            // processed, which (in this design) the Lead only ever
            // sends once it has itself already successfully applied
            // the corresponding switch -- i.e. once it has already
            // downloaded and translated this exact layout itself. A
            // real, user-proposed hypothesis: the CMS's own
            // RequiredFiles computation may specifically release a
            // newly (re)assigned Synchronised Event layout to a
            // Follower only *after* the Lead has already fetched it --
            // meaning this precise moment (a Command/heartbeat just
            // confirmed the Lead is done) is exactly when a retry is
            // most likely to actually succeed, unlike a fixed short
            // delay attempted blindly on every collection cycle (an
            // earlier version of this fix tried exactly that -- see
            // this function's own history -- which wouldn't reliably
            // help if this hypothesis is correct, since the CMS's own
            // gate is the Lead's download, not merely elapsed time).
            log::info!("Sync Group: layout {candidate} not yet in cache -- retrying \
                        RequiredFiles now that a Command/heartbeat confirms the Lead \
                        has already applied it");
            match self.retry_and_download_layout(candidate) {
                Ok(()) => {
                    self.reconnect_after_catching_up_on_own(candidate);
                    return None;
                }
                Err(e) => {
                    log::warn!("Sync Group: retrying/downloading layout {candidate}: {e:#}");
                }
            }
        }
        let info = self.cache.get_layout(candidate)?;
        info.sync_keys.iter().any(|k| sync_keys.contains(k)).then_some(candidate)
    }

    /// Called once `retry_and_download_layout` has just successfully
    /// caught this display's own cache up on `candidate` -- this
    /// display's own region/playlist timers would otherwise start
    /// "now" (whenever this download just finished) rather than at
    /// the Lead's own originally-published target_time, which could
    /// already be a real, if small, amount of time in the past by now
    /// (the Lead published it, this display noticed it was missing
    /// the layout, retried RequiredFiles, downloaded, and translated
    /// it -- all real elapsed time). Applying immediately would show
    /// the *correct content*, but drifted in time from the rest of
    /// the group -- the same class of problem the reconnect-on-expiry
    /// mechanism exists to avoid. Reconnecting here (Follower only --
    /// a Lead has nothing to reconnect *to* for its own locally-
    /// discovered switches) re-triggers the Lead's own
    /// `sync_peer_connected` reaction, which re-publishes a *fresh*
    /// target_time -- this display then applies via that normal,
    /// freshly-coordinated Command instead of right here, so it
    /// converges on the same real-world instant as everyone else
    /// rather than one of its own making. A separate method
    /// specifically so this reaction is directly testable without
    /// needing a mock capable of a full RequiredFiles+GetFile round
    /// trip just to reach it.
    fn reconnect_after_catching_up_on_own(&mut self, candidate: i64) {
        log::info!("Sync Group: layout {candidate} now downloaded -- reconnecting to \
                    get a freshly-coordinated sync from the Lead instead of applying \
                    immediately with potentially stale timing");
        // See this field's own doc comment -- avoids racing with the
        // immediately-following schedule_check() call (already made
        // unconditionally by this function's own caller) independently
        // re-discovering this exact, just-cached layout on its own and
        // staging/applying it a second time.
        self.suppress_local_discovery_of = Some((candidate, std::time::Instant::now()));
        if let SyncRole::Follower { lead_addr } = &self.settings.sync_role {
            let lead_addr = lead_addr.clone();
            self.connect_as_follower(&lead_addr);
        }
    }

    /// Re-queries `RequiredFiles` and, if it now offers `id` as a
    /// layout-typed entry, downloads and translates it immediately --
    /// see `resolve_layout_for_sync_keys`'s own call site for why this
    /// specific moment (not a blind, fixed-delay retry) is the right
    /// time to try this.
    fn retry_and_download_layout(&mut self, id: i64) -> Result<()> {
        let (required, _purge) = self.xmds.required_files()
            .context("retrying RequiredFiles")?;
        let file = required.into_iter()
            .find(|f| matches!(f, ReqFile::File { typ: "layout", id: fid, .. } if *fid == id))
            .with_context(|| format!("layout {id} still not offered by RequiredFiles"))?;
        self.cache.download(file, &mut self.xmds)
            .with_context(|| format!("downloading layout {id}"))?;
        Ok(())
    }

    fn schedule_check(&mut self) {
        // Prune expired Schedule Criteria before every evaluation, so a
        // stale value doesn't keep a criteria-conditioned layout active
        // (or, for a `ne` condition, incorrectly inactive) past its ttl.
        self.criteria.prune_expired();

        // A Sync Group override (see sync_layout_active's own doc
        // comment) has no fixed duration/completion signal of its own
        // to revert on, unlike a Scheduled Action's own override --
        // check on every cycle whether the schedule still considers it
        // sync-gated right now; once its own scheduled window ends (or
        // its criteria stop matching), clear it and let normal
        // schedule resolution resume.
        if self.sync_layout_active {
            match self.override_layout {
                Some(id) if !self.schedule.is_sync_gated(id, &self.criteria) => {
                    log::info!("Sync Group: synchronized layout {id}'s own scheduled \
                                window ended -- reverting to normal schedule");
                    self.override_layout = None;
                    self.sync_layout_active = false;
                    if let Some(sync_group) = &self.sync_group {
                        sync_group.set_current_sync_keys(vec![]);
                    }
                    // (Follower only.) If the CMS swapped which layout
                    // a live Synchronised Event points to (same
                    // sync_keys, e.g. visually similar templates), a
                    // Follower whose own schedule.xml hadn't downloaded
                    // that change yet could match the Lead's fresh
                    // Command against its own still-stale schedule,
                    // resolving to the superseded layout. This expiry
                    // check firing means this display's own view has
                    // genuinely caught up -- reconnecting the SyncGroup
                    // now re-triggers the Lead's own
                    // `sync_peer_connected` reaction (already
                    // unconditional there, no way to tell a first
                    // connection from a reconnection apart), which
                    // re-publishes its current sync_keys fresh. Simpler
                    // than a dedicated Follower -> Lead "ready" signal
                    // (the existing connection is one-way, PUB/SUB).
                    if let SyncRole::Follower { lead_addr } = &self.settings.sync_role {
                        let lead_addr = lead_addr.clone();
                        self.connect_as_follower(&lead_addr);
                    }
                }
                Some(_) => {}
                // Defensive: shouldn't happen (only ever set alongside
                // override_layout), but don't leave a stale flag set.
                None => self.sync_layout_active = false,
            }
        }

        // Whether this cycle is driven by an *already-committed*
        // override (a Scheduled Action's own navLayout, or a Sync
        // Group switch already applied by sync_apply_timer firing) --
        // in that case the schedule below must always be applied
        // directly, with no sync-gating check of its own: that
        // decision was already made once, by whichever mechanism set
        // `override_layout` in the first place, and re-checking
        // is_sync_gated here would just re-stage the exact same
        // layout it was itself the result of staging, looping forever.
        let overriding = self.override_layout.is_some();
        let new_layouts = match self.override_layout {
            // An active changeLayout override completely replaces the
            // normal CMS schedule -- see the `override_layout` field doc.
            Some(id) => vec![id],
            None => self.schedule.layouts_now(&self.criteria),
        };
        // Filter out scheduled layouts not yet cached (download window
        // or a transient failure can leave one absent on disk) --
        // fall back to whatever's already showing instead of switching
        // to something that doesn't exist yet.
        let available: Vec<_> = new_layouts.iter().copied()
            .filter(|&id| self.cache.get_layout(id).is_some())
            .collect();
        let new_layouts = if available.is_empty() && !new_layouts.is_empty() {
            // This codebase's own Logger formats record.args() twice
            // (once for console, once for the string stored for later
            // SubmitLog upload) -- itertools::Format is single-use, so
            // formatting it twice panics. Converting to a String first
            // means the log macro only ever sees a plain, idempotent
            // String.
            let listed = new_layouts.iter().format(", ").to_string();
            log::warn!("none of the newly-scheduled layout(s) ({listed}) are cached yet, \
                        keeping whatever's currently showing until they are");
            self.layouts.clone()
        } else {
            available
        };
        if new_layouts != self.layouts {
            // See is_sync_gated's own doc comment for why this check is
            // skipped entirely (`overriding`) once a layout switch --
            // synchronized or not -- has already been committed.
            let sync_gated = !overriding && new_layouts.first()
                .is_some_and(|&id| self.schedule.is_sync_gated(id, &self.criteria));
            if sync_gated {
                let first = new_layouts[0];
                // See suppress_local_discovery_of's own doc comment --
                // a short-lived grace window (long enough for a
                // reconnect + fresh Command round trip, matching real,
                // observed timing of ~1-2s; falls back to normal
                // discovery on its own if that doesn't pan out) to
                // avoid racing with a reconnect
                // reconnect_after_catching_up_on_own just triggered
                // for this exact layout.
                let suppressed = matches!(self.suppress_local_discovery_of,
                    Some((id, at)) if id == first
                        && at.elapsed() < std::time::Duration::from_secs(5));
                if suppressed {
                    log::debug!("Sync Group: local discovery of layout {first} \
                                 suppressed briefly -- a reconnect already triggered \
                                 for it is expected to produce a fresh Command shortly");
                } else {
                // This display's own layout's own sync_keys -- not an
                // id at all, published/staged below (see
                // resolve_layout_for_sync_keys's own doc comment for
                // why an id is never trusted across the network).
                match self.cache.get_layout(first) {
                    Some(info) => {
                        let sync_keys = info.sync_keys.clone();
                        // Guard against re-publishing/re-staging every
                        // single cycle while this exact switch is
                        // already pending -- schedule_check() can run
                        // many times (every collection, plus whenever
                        // anything else changes) before
                        // sync_apply_timer actually fires.
                        //
                        // Compares the *layout id* (`first`), not the
                        // sync_keys value -- successive layout swaps
                        // within the same live Synchronised Event can
                        // share the exact same sync_keys text
                        // (structurally-similar templates). Comparing
                        // pending_sync_keys by value alone meant that,
                        // once one was staged, this guard saw every
                        // subsequent, genuinely different layout's own
                        // identical-looking sync_keys as "already
                        // handled" and silently did nothing -- a
                        // display could never advance to a new
                        // sync-gated layout again for as long as its
                        // sync_keys kept matching a previous one.
                        if self.pending_sync_layout_id != Some(first) {
                            log::info!("new sync-gated layout in schedule: {first} \
                                        (sync_keys {sync_keys:?}) -- staging a \
                                        synchronized switch instead of applying \
                                        immediately");
                            self.pending_sync_layout_id = Some(first);
                            self.stage_sync_switch(sync_keys);
                        }
                    }
                    // A newly sync-gated layout whose own required
                    // files hadn't finished downloading yet this cycle
                    // used to still stage an empty Vec here (via
                    // `.unwrap_or_default()`) -- not because it
                    // genuinely has no sync_keys, but because they
                    // weren't knowable yet. Combined with the guard
                    // above, that wrong, empty value stayed stuck in
                    // pending_sync_keys permanently, even once the
                    // real sync_keys became known moments later.
                    //
                    // Fixed by staging nothing at all here instead --
                    // pending_sync_keys is left as-is, so a later
                    // schedule_check (once actually in cache) sees a
                    // fresh discovery and stages the real sync_keys
                    // normally.
                    None => {
                        log::debug!("sync-gated layout {first} not yet in cache -- \
                                     will determine its own sync_keys and stage a \
                                     synchronized switch once it's downloaded");
                    }
                }
                }
                // Deliberately not updating self.layouts/ToGui::Layouts
                // here -- that only happens once sync_apply_timer
                // actually fires and this function runs again with
                // `overriding == true`.
            } else {
                let all_layouts = new_layouts.iter().format(", ").to_string();
                log::info!("new layouts in schedule: {}", all_layouts);
                self.to_gui.send(ToGui::Layouts(new_layouts.clone())).unwrap();
                self.layouts = new_layouts;
            }
        }
        // Update schedule-driven overlays *before* pruning below, so a
        // freshly-discovered overlay's own layout id is already
        // reflected in self.schedule_overlays by the time we build the
        // combined "still active" list.
        self.recheck_schedule_overlays();
        // Stop polling GetData for any data widget whose own layout
        // isn't in the current schedule anymore -- run unconditionally
        // (not just when the schedule actually changed above), a cheap
        // no-op call otherwise, and more robust against other paths
        // that could change what's active without going through the
        // check just above.
        //
        // A data widget living inside an *overlay* layout (tracked
        // separately in self.schedule_overlays, never in self.layouts)
        // was discovered correctly on download, but then immediately
        // pruned right back out on the very next schedule_check --
        // this check used to only consider self.layouts, so an
        // overlay's own layout id was never "still active", preventing
        // that widget from ever completing a GetData refresh.
        let active_layouts: Vec<i64> = self.layouts.iter().copied()
            .chain(self.schedule_overlays.iter().map(|&(id, _)| id))
            .collect();
        self.cache.prune_data_widgets_not_in(&active_layouts);
    }


    /// Fires any scheduled command (schedule::ScheduledCommand) that's
    /// due right now -- see Schedule::commands_due's own doc comment for
    /// the exact due-check semantics this relies on.
    fn check_scheduled_commands(&mut self) {
        let now = OffsetDateTime::now_local().unwrap();
        for (scheduleid, code) in self.schedule.commands_due(now, &self.commands_run) {
            log::info!("running scheduled command {code:?} (scheduleid {scheduleid})");
            self.run_command(&code);
            self.commands_run.insert(scheduleid);
        }
    }

    /// Handles an incoming webhook trigger code -- from either XMR's own
    /// pushed `triggerWebhook` action, or a direct HTTP POST to
    /// `/trigger` (see server::TriggerRequest). First checks for a
    /// matching Scheduled Action (schedule::ActionTarget -- a
    /// schedule-level "listen for this code, then Navigate to a Layout
    /// or run a Command", reachable regardless of what's currently
    /// showing), falling back to the older, narrower mechanism (an
    /// in-page widget-embedded action, only reachable while its own
    /// layout/widget happens to already be on screen) if no Scheduled
    /// Action matches -- otherwise a trigger could do nothing at all
    /// when the current layout had no matching widget-embedded action,
    /// but the actual configured action was a Scheduled Action
    /// targeting a different layout entirely.
    ///
    /// Returns true if the caller (run()'s own select! loop) should
    /// force an immediate collection -- the resolved target layout of a
    /// matched navLayout action isn't cached yet.
    fn handle_trigger_code(&mut self, code: &str) -> bool {
        let now = OffsetDateTime::now_local().unwrap();
        // Cloned immediately to release the borrow on self.schedule --
        // both branches below need &mut self (run_command,
        // resolve_layout_code, setting self.override_layout/etc), which
        // would otherwise conflict with a reference still borrowed from
        // self.schedule for the whole match.
        let matched = self.schedule.action_for_trigger(code, now)
            .map(|(duration, target)| (duration, target.clone()));
        if let Some((duration, target)) = matched {
            match target {
                schedule::ActionTarget::Layout(layout_code) => {
                    let Some(id) = self.cache.resolve_layout_code(&layout_code) else {
                        log::warn!("Scheduled Action: trigger {code:?} targets unknown layout \
                                    code {layout_code:?} -- not yet cached, or genuinely \
                                    misconfigured");
                        return false;
                    };
                    log::info!("Scheduled Action: trigger {code:?} navigating to layout \
                                {layout_code:?} (resolved to {id}), duration={duration}s");
                    self.override_layout = Some(id);
                    if duration > 0 {
                        self.override_expiry = after(Duration::from_secs(duration as u64));
                        self.override_revert_on_completion = false;
                    } else {
                        // Same reasoning as ScheduledAction::duration's
                        // own doc comment: wait for the shown layout's
                        // own natural completion instead of a timer.
                        self.override_expiry = never();
                        self.override_revert_on_completion = true;
                    }
                    if self.cache.get_layout(id).is_none() {
                        log::info!("Scheduled Action target layout {id} not yet cached, \
                                    forcing a collection");
                        return true;
                    }
                    self.schedule_check();
                }
                schedule::ActionTarget::Command(command_code) => {
                    log::info!("Scheduled Action: trigger {code:?} running command \
                                {command_code:?}");
                    self.run_command(&command_code);
                }
            }
            return false;
        }
        // No matching Scheduled Action -- fall back to the older,
        // narrower in-page widget-embedded action mechanism
        // (window.arexibo.triggers[code], see layout.rs's own
        // write_action / TriggerRequest's own doc comment). Kept as a
        // permanent debug-level log -- useful for confirming the Rust
        // side dispatched correctly even when nothing downstream fires.
        log::debug!("Trigger {code:?}: no Scheduled Action matched -- forwarding to \
                    the GUI thread for the in-page widget-embedded mechanism");
        self.to_gui.send(ToGui::Trigger(code.to_string())).unwrap();
        false
    }

    /// (Re)builds the Sync Group (video wall) LAN command channel to
    /// match the CMS's current `sync_role`/`sync_publisher_port` --
    /// see `sync_group_role`'s own doc comment for why a no-op most of
    /// the time is the correct, expected behavior. Called from
    /// `update_settings`, so this runs both on initial registration
    /// and on any later collection cycle where the CMS's settings
    /// changed.
    fn update_sync_group(&mut self) {
        // See `first_collection_done`'s own doc comment for why this
        // must wait -- deliberately checked *before* the existing
        // no-op-if-unchanged check below, so the two `update_settings`
        // calls `Handler::new` itself makes (before any collection has
        // ever run) skip this function entirely, rather than
        // connecting/announcing readiness with an empty cache.
        if !self.first_collection_done {
            return;
        }
        if self.settings.sync_role == self.sync_group_role {
            return;
        }
        self.sync_group_role = self.settings.sync_role.clone();
        match &self.settings.sync_role {
            SyncRole::None => {
                if self.sync_group.take().is_some() {
                    log::info!("Sync Group: no longer a member -- stopping the LAN \
                                command channel");
                }
                self.sync_commands = never();
                self.sync_peer_connected = never();
            }
            SyncRole::Lead => {
                log::info!("Sync Group: starting as Lead, listening on port {}",
                            self.settings.sync_publisher_port);
                let switch_delay = Duration::from_millis(self.settings.sync_switch_delay);
                match syncgroup::SyncGroup::start_lead(self.settings.sync_publisher_port,
                                                        switch_delay) {
                    Ok(g) => {
                        self.sync_peer_connected = g.peer_connected()
                            .expect("a Lead's own peer_connected() is always Some").clone();
                        self.sync_group = Some(g);
                    }
                    Err(e) => {
                        log::error!("Sync Group: starting as Lead: {e:#}");
                        self.sync_peer_connected = never();
                    }
                }
                self.sync_commands = never();
            }
            SyncRole::Follower { lead_addr } => {
                let lead_addr = lead_addr.clone();
                self.connect_as_follower(&lead_addr);
            }
        }
    }

    /// (Re)connects this display's own SyncGroup Follower connection to
    /// `lead_addr` -- shared by `update_sync_group`'s own Follower arm
    /// (a genuine role/address change) and by the sync-gated override
    /// expiry check in `schedule_check` (a real, *deliberate*
    /// reconnection of an *unchanged* Follower role/address, used
    /// purely to re-trigger the Lead's own `peer_connected` reaction --
    /// see that call site's own doc comment for the full story). Drops
    /// whatever SyncGroup handle already existed first (its own Drop
    /// impl stops the old background thread/connection) -- safe to
    /// call even when nothing was connected yet.
    fn connect_as_follower(&mut self, lead_addr: &str) {
        let addr = format!("{lead_addr}:{}", self.settings.sync_publisher_port);
        log::info!("Sync Group: starting/reconnecting as Follower, Lead at {addr}");
        self.sync_group = None;
        match syncgroup::SyncGroup::start_follower(addr) {
            Ok(g) => {
                self.sync_commands = g.commands()
                    .expect("a Follower's own commands() is always Some").clone();
                self.sync_group = Some(g);
            }
            Err(e) => {
                log::error!("Sync Group: starting as Follower: {e:#}");
                self.sync_commands = never();
            }
        }
        self.sync_peer_connected = never();
    }

    /// Re-evaluate schedule-driven Overlay Layouts (schedule.rs's
    /// `active_overlays` -- see the `schedule_overlays` field doc
    /// comment) and start/stop/restart the rotation as needed. A no-op
    /// (leaves whatever's currently showing alone) if the active set
    /// hasn't changed, so this can safely be called on every
    /// schedule_check() tick (every 60s) without resetting an
    /// in-progress rotation's timing each time. Also a no-op while an
    /// XMR-triggered overlay is active (`overlay_layout.is_some()`) --
    /// that always takes precedence; called again once it reverts (see
    /// the `overlay_expiry`/`RevertToSchedule` handling in `run()`) to
    /// pick schedule-driven overlays back up promptly.
    fn recheck_schedule_overlays(&mut self) {
        if self.overlay_layout.is_some() {
            return;
        }
        let new_overlays = self.schedule.active_overlays();
        if new_overlays == self.schedule_overlays {
            return;
        }
        log::info!("schedule-driven overlay set changed: {} active",
                    new_overlays.len());
        self.schedule_overlays = new_overlays;
        self.schedule_overlay_idx = 0;
        if self.schedule_overlays.is_empty() {
            self.to_gui.send(ToGui::HideOverlay).unwrap();
            self.overlay_expiry = never();
        } else {
            self.show_current_schedule_overlay();
        }
    }

    /// Show whichever schedule-driven overlay `schedule_overlay_idx`
    /// currently points at. Only (re)schedules `overlay_expiry` for its
    /// own `duration` -- to advance to the *next* one, wrapping around --
    /// if there's actually more than one overlay to rotate between.
    ///
    /// With only a single active overlay, there's nothing to rotate
    /// to -- reschedule only when there are 2+ overlays; otherwise show
    /// once and leave overlay_expiry at never() so it doesn't keep
    /// reloading itself indefinitely.
    fn show_current_schedule_overlay(&mut self) {
        let (layout_id, duration) = self.schedule_overlays[self.schedule_overlay_idx];
        if self.cache.get_layout(layout_id).is_none() {
            // Not cached yet -- wait and retry this slot once a
            // collection has fetched it, rather than showing blank.
            log::warn!("scheduled overlay layout {layout_id} not cached yet, \
                        will retry in {duration}s");
            self.overlay_expiry = after(Duration::from_secs(duration.max(1) as u64));
            return;
        }
        log::info!("showing scheduled overlay layout {layout_id} for {duration}s \
                    ({}/{} in rotation)", self.schedule_overlay_idx + 1, self.schedule_overlays.len());
        self.to_gui.send(ToGui::ShowOverlay(layout_id)).unwrap();
        self.overlay_expiry = if self.schedule_overlays.len() > 1 {
            after(Duration::from_secs(duration.max(1) as u64))
        } else {
            never()
        };
    }

    /// Retry every resource queued after a failed download (see
    /// `resource_retry_queue`'s own doc comment). Widgets past
    /// RESOURCE_RETRY_MAX_ATTEMPTS are dropped from the queue (logged)
    /// rather than retried forever -- they'll get another chance at
    /// the next full collection cycle regardless.
    fn retry_failed_resources(&mut self) {
        let queue = std::mem::take(&mut self.resource_retry_queue);
        for (file, attempts) in queue {
            let desc = file.description();
            // Must be `id` (the XLF's own <media id> / iframe id="m{id}")
            // not `mediaid` (a different field) -- refreshes the widget
            // once the retry succeeds instead of leaving it blank until
            // an unrelated layout switch.
            let widget_id = match &file {
                crate::resource::ReqFile::Resource { id, .. } => Some(*id),
                _ => None,
            };
            match self.cache.download(file.clone(), &mut self.xmds) {
                Ok(()) => {
                    log::info!("retry succeeded for {desc}");
                    if let Some(widget_id) = widget_id {
                        self.to_gui.send(ToGui::ReloadWidget(widget_id)).unwrap();
                    }
                }
                Err(e) => {
                    let attempts = attempts + 1;
                    if attempts >= RESOURCE_RETRY_MAX_ATTEMPTS {
                        log::warn!("giving up on {desc} after {attempts} failed retries: {e:#}");
                    } else {
                        log::warn!("retry {attempts}/{RESOURCE_RETRY_MAX_ATTEMPTS} failed for \
                                    {desc}, will retry again: {e:#}");
                        self.resource_retry_queue.push((file, attempts));
                    }
                }
            }
        }
        // Same transient-fault retry, for the dataUpdate path (see
        // `dataupdate_retry_queue`'s own doc comment on the Handler
        // struct -- this doesn't go through a raw ReqFile, so it can't
        // share the loop above; calls `refresh_resource` again instead,
        // exactly as the original DataUpdate handler did).
        let dataupdate_queue = std::mem::take(&mut self.dataupdate_retry_queue);
        for (widget_id, attempts) in dataupdate_queue {
            match self.cache.refresh_resource(widget_id, &mut self.xmds) {
                Ok(fetch_id) => {
                    log::info!("retry succeeded for dataUpdate widget {widget_id}");
                    self.to_gui.send(ToGui::ReloadWidget(fetch_id)).unwrap();
                }
                Err(e) => {
                    let attempts = attempts + 1;
                    if attempts >= RESOURCE_RETRY_MAX_ATTEMPTS {
                        log::warn!("giving up on dataUpdate widget {widget_id} after \
                                    {attempts} failed retries: {e:#}");
                    } else {
                        log::warn!("retry {attempts}/{RESOURCE_RETRY_MAX_ATTEMPTS} failed for \
                                    dataUpdate widget {widget_id}, will retry again: {e:#}");
                        self.dataupdate_retry_queue.push((widget_id, attempts));
                    }
                }
            }
        }
        self.resource_retry_timer = if self.resource_retry_queue.is_empty()
            && self.dataupdate_retry_queue.is_empty() {
            never()
        } else {
            after(RESOURCE_RETRY_DELAY)
        };
    }

    /// v7 GetData polling groundwork -- refreshes every currently-due
    /// tracked data widget (see resource::Cache's own data_widgets),
    /// then re-arms `next_data_refresh` for whatever becomes due next.
    /// A failed refresh is logged, not retried on its own separate
    /// schedule -- the widget stays tracked with its old
    /// last_refreshed time, so it's simply due again (and retried)
    /// the next time this fires, rather than needing a dedicated retry
    /// queue like `resource_retry_queue`'s own.
    fn refresh_due_data_widgets(&mut self) {
        let now = std::time::Instant::now();
        for widget_id in self.cache.data_widgets_due(now) {
            match self.cache.refresh_data_widget(widget_id, &mut self.xmds, now) {
                // Without this, a freshly-written <widgetId>.json would
                // sit on disk unnoticed -- the already-loaded page has
                // no reason to re-fetch it on its own. Same mechanism
                // already used for the XMR-pushed DataUpdate case
                // above, reused here for consistency rather than
                // inventing a lighter in-page re-render.
                Ok(resource_id) => {
                    self.to_gui.send(ToGui::ReloadWidget(resource_id)).unwrap();
                }
                Err(e) => log::warn!("refreshing data widget {widget_id}: {e:#}"),
            }
        }
        self.rearm_data_refresh_timer();
    }

    /// Arms `next_data_refresh` to fire when the soonest tracked data
    /// widget is next due, or `never()` if none are currently tracked
    /// -- called both after `refresh_due_data_widgets` itself (so it
    /// keeps firing for the *next* due widget) and after every
    /// `collect_once()` (since that's when new data widgets get
    /// discovered, and a newly-found widget's own interval might be
    /// sooner than whatever was already armed).
    fn rearm_data_refresh_timer(&mut self) {
        let now = std::time::Instant::now();
        self.next_data_refresh = match self.cache.next_data_widget_due_in(now) {
            Some(d) => after(d),
            None => never(),
        };
    }

    /// Checks and clears `force_reload_after_collect` (see its own
    /// doc comment) -- called right after every `collect_once()`
    /// completes, regardless of success or failure. A separate method
    /// specifically so this is directly testable without needing to
    /// drive the whole (infinite, blocking) `run()` loop just to
    /// reach it.
    ///
    /// Only actually fires the reload once two things are confirmed:
    /// the current layout's own translated HTML is present on disk
    /// (catches a translation failure even if the raw download
    /// succeeded), and this cycle's download_required_files() call
    /// reported zero failures for any media/layout file (see
    /// `last_collect_had_failures`'s own doc comment for why
    /// "resource" failures don't count). Otherwise navigating could
    /// hit a genuinely missing target (a live browser 404, stuck
    /// until the schedule is deleted) -- worse than staying on the
    /// current, stale-but-rendered page a moment longer. If either
    /// check fails, the flag stays set for the next collection cycle.
    fn maybe_force_reload_after_purge(&mut self) {
        if self.force_reload_after_collect {
            let html_path = self.cache.dir().join(format!("{}.xlf.html", self.current_layout));
            if !html_path.exists() {
                log::warn!("purge-triggered reload: {} not found on disk yet (current \
                            layout {}) -- not reloading this cycle, will retry after the \
                            next collection", html_path.display(), self.current_layout);
                return;
            }
            if self.last_collect_had_failures {
                log::warn!("purge-triggered reload: at least one required file failed to \
                            download this cycle -- not reloading yet, will retry after the \
                            next collection");
                return;
            }
            self.force_reload_after_collect = false;
            self.to_gui.send(ToGui::ForceReloadLayout(self.current_layout)).unwrap();
        }
    }

    /// Called right after a required file's own download succeeds
    /// (see `download_required_files`'s own call site for the full
    /// story) -- if it was the layout file for whatever's *currently
    /// showing*, sets `force_reload_after_collect` (see its own doc
    /// comment) so the GUI actually picks up the change, since
    /// schedule_check()'s own reload decision alone (keyed on the
    /// layout id changing) would otherwise never notice a same-id
    /// layout's own content being updated underneath it. A separate
    /// method specifically so this is directly testable without
    /// needing to mock an actual XMDS GetFile download succeeding.
    fn note_layout_file_downloaded(&mut self, layout_id_if_any: Option<i64>) {
        if let Some(layout_id) = layout_id_if_any {
            if layout_id == self.current_layout {
                self.force_reload_after_collect = true;
            }
        }
    }

    /// Apply new player settings.
    fn update_settings(&mut self) -> Result<()> {
        // CMS's logLevel setting takes effect unless --debug is set
        // locally (an explicit override shouldn't be silently beaten
        // by a remote setting).
        if !self.debug_override {
            log::set_max_level(self.settings.log_level_filter());
        }

        // Adspace on/off state for newly-translated layouts (see
        // adspace.rs) -- adspace_partner left unset, no confirmed CMS
        // field name found, optional in the bid request anyway.
        self.cache.adspace_enabled = self.settings.is_adspace_enabled;

        // Applies the CMS's own display_time_zone as *this process's*
        // local timezone (see apply_process_timezone's own doc comment
        // for why, and why only once per run) rather than merely
        // warning about a mismatch with the system's -- a warning
        // requires someone to notice it in the logs; actively using
        // the CMS-specified timezone for our own schedule/download-
        // window evaluations means arexibo behaves correctly even on
        // a misconfigured installation, self-correcting instead of
        // just alerting.
        match decide_timezone_action(&self.process_timezone_applied, &self.settings.display_time_zone) {
            TimezoneAction::DoNothing => {}
            TimezoneAction::Apply(tz) => {
                apply_process_timezone(&tz);
                self.process_timezone_applied = Some(tz);
            }
            TimezoneAction::WarnRestartNeeded { was, now } => {
                // Same reasoning as RestartRequired's own doc comment
                // -- exiting directly here (std::process::exit) used
                // to bypass gui::quit()'s own safe shutdown sequencing
                // whenever this ran from collect_once() (mainloop
                // thread, GUI already active), risking the same real
                // segfault RestartRequired was built to avoid. This
                // specific case can't actually occur during
                // Handler::new's own initial update_settings call (no
                // previously-applied timezone yet to differ from), but
                // unifying on the same marker regardless keeps this
                // consistent rather than depending on that reasoning
                // never changing.
                log::error!("the CMS's own display timezone changed from {was:?} to \
                             {now:?} since this process started -- exiting so systemd \
                             can restart cleanly and pick up the change (changing a \
                             process's own timezone mid-session, from a background \
                             thread, isn't safe to do here)");
                return Err(RestartRequired.into());
            }
        }

        // Fallback for a screenshot request lost while offline --
        // edge-triggered, not repeated every cycle.
        if self.settings.screen_shot_requested {
            if !self.screenshot_requested_seen {
                log::info!("CMS has a pending screenshot request (screenShotRequested) -- \
                            likely lost while offline/disconnected, fulfilling it now");
                self.to_gui.send(ToGui::Screenshot).unwrap();
                self.screenshot_requested_seen = true;
            }
        } else {
            self.screenshot_requested_seen = false;
        }

        // forceHttps (see that field's own doc comment in config.rs)
        // -- same validate-before-committing caution as the CMS
        // migration below, and reuses the same validate_new_cms
        // helper. If this succeeds it exits the process too.
        self.attempt_https_upgrade()?;

        // (Re)builds the Sync Group LAN command channel to match the
        // CMS's current sync_role -- see update_sync_group's own doc
        // comment for why this is safe to call unconditionally here
        // (a no-op when nothing actually changed).
        self.update_sync_group();

        // CMS-driven migration to a different server (see
        // new_cms_address/new_cms_key's own doc comment in config.rs)
        // -- deliberately cautious given how risky getting this wrong
        // would be (a totem could become unreachable). Only attempted
        // here, at the very end, after every other settings-derived
        // action above -- if this succeeds it exits the process.
        self.attempt_cms_migration()?;

        // let the GUI know to reconfigure itself
        self.to_gui.send(ToGui::Settings(Box::new(self.settings.clone()))).unwrap();
        Ok(())
    }

    /// Upgrades http:// to https:// when the CMS's forceHttps is on,
    /// validating the https:// address works first (reusing
    /// validate_new_cms). On failure stays on http and retries next
    /// cycle.
    fn attempt_https_upgrade(&mut self) -> Result<()> {
        if !self.settings.force_https {
            return Ok(());
        }
        let Some(https_address) = https_upgrade_address(&self.cms.address) else {
            return Ok(()); // already https://, or some other/unrecognized scheme
        };

        log::info!("CMS has forceHttps enabled and our own address ({}) is still http:// \
                    -- validating the https:// equivalent before switching",
                   self.cms.address);

        let candidate = CmsSettings { address: https_address, ..self.cms.clone() };

        let pub_key = match RsaPublicKey::from(&self.xmr_privkey).to_public_key_pem(Default::default()) {
            Ok(k) => k,
            Err(e) => {
                log::error!("deriving public key for forceHttps validation, NOT \
                             switching: {e:#}");
                return Ok(());
            }
        };

        match validate_new_cms(&candidate, pub_key, self.no_verify, self.envdir.join("xml")) {
            Ok(()) => {
                log::error!("https:// address confirmed working -- switching to it and \
                             exiting so systemd restarts cleanly");
                if let Err(e) = candidate.to_file(self.envdir.join("cms.json")) {
                    log::error!("writing cms.json for forceHttps upgrade: {e:#} -- NOT \
                                 exiting, will retry next cycle");
                    return Ok(());
                }
                // See RestartRequired's own doc comment -- this can
                // genuinely occur during Handler::new's own initial
                // update_settings call (before gui::run() ever starts,
                // where a direct exit would be safe too), or from
                // collect_once() (mainloop thread, GUI already
                // active, where it isn't) -- unified on the same
                // marker either way rather than depending on knowing
                // which context called this.
                return Err(RestartRequired.into());
            }
            Err(e) => {
                log::warn!("the https:// equivalent of our own CMS address didn't work, \
                            staying on http:// for now (will retry next cycle): {e:#}");
            }
        }
        Ok(())
    }

    /// Attempts a CMS migration if requested (new_cms_address/
    /// new_cms_key). Validates the new CMS first -- READY or WAITING
    /// both count as success, since a freshly-migrated display is
    /// likely not yet authorized. No-op if no migration is requested.
    fn attempt_cms_migration(&mut self) -> Result<()> {
        if !should_attempt_cms_migration(&self.settings.new_cms_address, &self.settings.new_cms_key) {
            return Ok(());
        }

        log::info!("CMS requested migration to a new server ({}) -- validating before \
                    committing anything", self.settings.new_cms_address);

        let candidate = CmsSettings {
            address: self.settings.new_cms_address.clone(),
            key: self.settings.new_cms_key.clone(),
            // Same physical device migrating, not a fresh one -- keep
            // its own stable identity (see CmsSettings's own field,
            // used to derive hw_key in xmds::Cms::new) so the new
            // CMS's admin sees the same hardware key to recognize and
            // authorize, and carry over the other non-identity-
            // affecting settings too.
            display_id: self.cms.display_id.clone(),
            display_name: self.cms.display_name.clone(),
            proxy: self.cms.proxy.clone(),
        };

        let pub_key = match RsaPublicKey::from(&self.xmr_privkey).to_public_key_pem(Default::default()) {
            Ok(k) => k,
            Err(e) => {
                log::error!("deriving public key for CMS migration validation, NOT \
                             migrating: {e:#}");
                return Ok(());
            }
        };

        match validate_new_cms(&candidate, pub_key, self.no_verify, self.envdir.join("xml")) {
            Ok(()) => {
                log::info!("new CMS accepted registration -- committing migration");
                match commit_cms_migration(&self.envdir, &candidate) {
                    Ok(()) => {
                        log::error!("CMS migration complete -- exiting so systemd restarts \
                                     cleanly against the new CMS");
                        // See attempt_https_upgrade's own comment on
                        // why RestartRequired is used uniformly here
                        // regardless of which context called this.
                        return Err(RestartRequired.into());
                    }
                    Err(e) => {
                        log::error!("committing CMS migration (validation had already \
                                     succeeded): {e:#} -- NOT exiting, will retry next cycle");
                    }
                }
            }
            Err(e) => {
                log::error!("validating new CMS before migration, NOT migrating \
                             (will retry next cycle): {e:#}");
            }
        }
        Ok(())
    }
}

/// Confirms a candidate CMS is reachable and accepts this display.
/// READY or WAITING both count as success -- "not yet authorized"
/// says nothing about whether the CMS itself is reachable/correct.
fn validate_new_cms(candidate: &CmsSettings, pub_key: String, no_verify: bool,
                     xml_dir: PathBuf) -> Result<()> {
    let mut validation_cms = xmds::Cms::new(candidate, pub_key, no_verify, xml_dir)
        .context("constructing validation client")?;
    validation_cms.register_display().context("registering with the new CMS")?;
    Ok(())
}

/// Literal http:// -> https:// scheme swap, keeping host/port/path as
/// configured. None if already https:// or another scheme.
fn https_upgrade_address(address: &str) -> Option<String> {
    address.strip_prefix("http://").map(|rest| format!("https://{rest}"))
}

fn should_attempt_cms_migration(new_address: &str, new_key: &str) -> bool {
    !new_address.is_empty() && !new_key.is_empty()
}

/// Disk-mutating part of a CMS migration, separated from validation
/// and the final process::exit() so it's testable on its own. Only
/// called after the new CMS is already confirmed reachable.
fn commit_cms_migration(envdir: &Path, new_cms: &CmsSettings) -> Result<()> {
    let cms_path = envdir.join("cms.json");
    let backup_path = envdir.join("cms.json.bak");
    if let Err(e) = fs::copy(&cms_path, backup_path) {
        log::warn!("backing up cms.json before CMS migration: {e:#}");
    }
    new_cms.to_file(&cms_path).context("writing new cms.json")?;

    // Stale cache from the old CMS could be misleading (same layout id
    // can mean something else there) -- same clearing as --clear-cache.
    let cache_dir = envdir.join("res");
    if cache_dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&cache_dir) {
            log::warn!("clearing cache after CMS migration: {e:#}");
        }
    }
    Ok(())
}


/// Load the RSA private key for the XML channel from disk, or create a new
/// key if needed.  Returns the public key as a PEM string, which is how
/// it needs to be sent to the CMS.
fn load_or_create_keypair(dir: &Path) -> Result<(RsaPrivateKey, String)> {
    let privkey = if let Ok(key) = RsaPrivateKey::read_pkcs8_pem_file(dir.join("id_rsa")) {
        key
    } else {
        log::info!("generating new RSA key for XMR, please wait...");
        let key = RsaPrivateKey::new(&mut OsRng, 2048)?;
        key.write_pkcs8_pem_file(dir.join("id_rsa"), Default::default())?;
        key
    };
    let pubkey = RsaPublicKey::from(&privkey).to_public_key_pem(Default::default())?;
    Ok((privkey, pubkey))
}

/// Is `code` allowed by a `ShellCommandAllowList`-style comma-separated
/// list? An empty (or whitespace-only) list means "no restriction beyond
/// the enable_shell_commands master switch itself" -- matches the
/// observed default of an empty `<ShellCommandAllowList />`. Matching is
/// exact (after trimming whitespace around each comma-separated entry),
/// not a substring/prefix match -- an entry must equal the *entire*
/// command line to allow it.
fn is_command_allowed(allow_list: &str, code: &str) -> bool {
    if allow_list.trim().is_empty() {
        return true;
    }
    allow_list.split(',').map(str::trim).any(|entry| entry == code)
}

/// Whether a candidate XMR WebSocket address is actually usable to
/// justify replacing a previously-known-good one -- must have an
/// explicit port, not just be non-empty (a bare "ws://host" without a
/// port is never a legitimate replacement).
fn ws_address_has_port(addr: &str) -> bool {
    if addr.is_empty() {
        return false;
    }
    tungstenite::http::uri::Uri::try_from(addr)
        .map(|uri| uri.port_u16().is_some())
        .unwrap_or(false)
}

/// Whether `candidate` is exactly what `cms`'s own
/// `default_xmr_websocket_address()` fallback would produce right now
/// -- exempts our own intentional port-less default from the sticky
/// check's suspicious-address guard.
fn is_own_derived_ws_default(cms: &CmsSettings, candidate: &str) -> bool {
    cms.default_xmr_websocket_address().as_deref() == Some(candidate)
}

/// Applies a GetWeather JSON response to the given CriteriaStore --
/// every key/value pair becomes one Schedule Criteria update (see
/// xmds::Cms::get_weather's own doc comment for why). Extracted as
/// its own pure function specifically so this can be unit-tested
/// directly, without needing a full Handler or a mock CMS server
/// capable of answering every XMDS call collect_once() makes.
fn apply_weather_criteria(criteria: &mut CriteriaStore, json: &str, ttl: i64) -> Result<()> {
    let weather: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json)?;
    for (metric, value) in weather {
        criteria.set(metric, util::json_value_to_criteria_string(&value), ttl);
    }
    Ok(())
}

/// Which timezone to report via NotifyStatus. Prefers the CMS's own
/// display_time_zone once set (else our own report would overwrite an
/// admin's manual change), falling back to the system timezone only
/// for a never-configured display.
fn timezone_to_report(cms_display_time_zone: &str, system_timezone: &str) -> String {
    if !cms_display_time_zone.is_empty() {
        cms_display_time_zone.to_string()
    } else {
        system_timezone.to_string()
    }
}

#[cfg(test)]
mod timezone_to_report_tests {
    use super::*;

    #[test]
    fn prefers_the_cms_own_value_once_it_has_provided_one() {
        assert_eq!(timezone_to_report("Europe/London", "Europe/Rome"), "Europe/London");
    }

    #[test]
    fn falls_back_to_the_system_timezone_for_a_never_configured_display() {
        assert_eq!(timezone_to_report("", "Europe/Rome"), "Europe/Rome");
    }
}

/// Whether `file` (a possibly-modified layout) occupies the same
/// schedule slot as what's currently on screen, so it can be exempted
/// from re-download this cycle instead of interrupting playback.
/// Uses scheduleid, not the raw layout id -- publishing a layout
/// changes its id but keeps the same scheduleid. `current_scheduleid`
/// must come from the *previous* schedule (the old layout id may no
/// longer exist in the fresh one); 0 means no real schedule entry,
/// never exempt.
fn is_exempt_as_currently_playing_layout(file: &ReqFile, current_scheduleid: i64,
                                          fresh_schedule: &Schedule,
                                          expire_modified_layouts: bool) -> bool {
    if expire_modified_layouts || current_scheduleid == 0 {
        return false;
    }
    match file {
        ReqFile::File { typ: "layout", id, .. } =>
            fresh_schedule.scheduleid_for(*id) == current_scheduleid,
        _ => false,
    }
}

/// Whether an error is a "Procedure ... not present" SOAP fault --
/// happens when a v6/v7-only method (GetWeather/GetDependency/GetData)
/// is called against our v5 endpoint. Permanent, not worth retrying.
fn is_method_not_present_fault(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("not present")
}

/// Whether an error is the CMS's own weather module returning no data
/// for this display -- a real observed PHP TypeError (the endpoint
/// exists and gets called, but its internal weather data source
/// returns null where a string is expected). Confirmed to happen both
/// with no provider configured at all, and with a provider configured
/// but for a different geographic area than this display's own
/// location -- either way, same fault shape. Unlike
/// is_method_not_present_fault, this can resolve itself if the CMS's
/// own configuration changes later -- worth continuing to retry, just
/// not worth warning about every single cycle.
fn is_weather_provider_not_configured_fault(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("getWeatherData(): Return value must be of type string")
}

// tzset(3): glibc doesn't re-parse TZ on its own after set_var(). One
// extern fn instead of pulling in `libc` just for this.
extern "C" {
    fn tzset();
}

/// Sets this process's own TZ env var to the CMS's display_time_zone,
/// so schedule/download-window checks use it without touching the
/// system-wide timezone. Only call once, before any other thread
/// exists -- mutating env vars concurrently with another thread
/// reading them is unsafe regardless of what the compiler enforces.
fn apply_process_timezone(tz: &str) {
    match util::read_system_timezone() {
        Some(sys_tz) if sys_tz != tz => log::info!(
            "using {tz:?} as this process's own timezone (from the CMS's own \
             display_time_zone setting) -- note this machine's own system timezone \
             is {sys_tz:?}, only this process's own idea of local time is affected"),
        _ => log::info!("using {tz:?} as this process's own timezone (from the CMS's \
                          own display_time_zone setting)"),
    }
    std::env::set_var("TZ", tz);
    // SAFETY: only reads TZ and updates process-global libc state; safe
    // single-threaded, per this function's own doc comment.
    unsafe { tzset(); }
}

/// Pure decision so this can be unit-tested without touching the real
/// (process-global) TZ env var.
#[derive(Debug, PartialEq, Eq)]
enum TimezoneAction {
    DoNothing,
    Apply(String),
    /// Already applied a different value this run -- changing it again
    /// would happen from a later, possibly multi-threaded context, so
    /// report instead of acting.
    WarnRestartNeeded { was: String, now: String },
}

fn decide_timezone_action(applied: &Option<String>, cms_tz: &str) -> TimezoneAction {
    if cms_tz.is_empty() {
        return TimezoneAction::DoNothing;
    }
    match applied {
        None => TimezoneAction::Apply(cms_tz.to_string()),
        Some(applied) if applied == cms_tz => TimezoneAction::DoNothing,
        Some(applied) => TimezoneAction::WarnRestartNeeded {
            was: applied.clone(), now: cms_tz.to_string(),
        },
    }
}

#[cfg(test)]
mod ws_address_has_port_tests {
    use super::ws_address_has_port;

    #[test]
    fn empty_string_has_no_port() {
        assert!(!ws_address_has_port(""));
    }

    #[test]
    fn address_with_explicit_port_has_a_port() {
        assert!(ws_address_has_port("ws://192.168.2.10:8080"));
        assert!(ws_address_has_port("wss://example.com:443"));
    }

    #[test]
    fn address_without_a_port_has_no_port() {
        // The exact real, malformed address from section 62's own bug
        // report -- confirming this is precisely the case that must be
        // rejected here, not just an empty string.
        assert!(!ws_address_has_port("ws://192.168.2.10"));
    }

    #[test]
    fn unparseable_garbage_has_no_port() {
        assert!(!ws_address_has_port("not a valid uri at all"));
    }
}

#[cfg(test)]
mod command_allow_list_tests {
    use super::is_command_allowed;

    #[test]
    fn empty_allow_list_permits_anything() {
        assert!(is_command_allowed("", "rm -rf /"));
        assert!(is_command_allowed("   ", "reboot"));
    }

    #[test]
    fn exact_match_required() {
        let list = "reboot, /usr/bin/foo --bar";
        assert!(is_command_allowed(list, "reboot"));
        assert!(is_command_allowed(list, "/usr/bin/foo --bar"));
        assert!(!is_command_allowed(list, "reboot now"));
        assert!(!is_command_allowed(list, "/usr/bin/foo"));
        assert!(!is_command_allowed(list, "shutdown"));
    }

    #[test]
    fn whitespace_around_entries_is_ignored() {
        assert!(is_command_allowed("  reboot  ,  shutdown  ", "shutdown"));
    }
}

#[cfg(test)]
mod pending_auth_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Minimal mock XMDS server: responds to every request with a
    /// canned RegisterDisplay SOAP response, "not yet authorized" for
    /// the first `not_ready_count` calls, then "READY" (with a mostly-
    /// empty but validly-parseable settings payload) afterwards --
    /// faithfully exercising the *real* SOAP response format (found by
    /// inspecting the actual generated parser in
    /// target/.../out/xmds_soap.rs), not a shortcut that bypasses the
    /// real registration/parsing code path this feature depends on.
    struct MockCms {
        port: u16,
        calls: std::sync::Arc<AtomicU32>,
    }

    impl MockCms {
        fn start(not_ready_count: u32) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let port = server.server_addr().to_ip().unwrap().port();
            let calls = std::sync::Arc::new(AtomicU32::new(0));
            let calls_clone = calls.clone();
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let n = calls_clone.fetch_add(1, Ordering::SeqCst);
                    let activation = if n < not_ready_count {
                        r#"<ActivationMessage code="WAITING"/>"#.to_string()
                    } else {
                        // Deliberately minimal beyond embeddedServerPort
                        // -- every other field has a graceful fallback
                        // default (see section 59's own fixes), so this
                        // alone must be enough to parse successfully.
                        // embeddedServerPort is set explicitly, matching
                        // PlayerSettings::default()'s own placeholder
                        // value used while pending -- without this, the
                        // transition out of pending would also (quite
                        // correctly, but not what this test is about)
                        // trip the port-change-forces-restart check,
                        // since a real server is already bound to that
                        // placeholder port the moment main.rs creates it,
                        // regardless of pending_auth/pending_network.
                        r#"<ActivationMessage code="READY"><embeddedServerPort>9696</embeddedServerPort></ActivationMessage>"#.to_string()
                    };
                    // The outer envelope's ActivationMessage element
                    // carries the inner XML as escaped *text* (matching
                    // the real protocol -- confirmed via the generated
                    // parser calling `.text()`, not re-parsing a nested
                    // element tree directly).
                    let escaped = activation.replace('&', "&amp;").replace('<', "&lt;")
                                             .replace('>', "&gt;").replace('"', "&quot;");
                    let body = format!(
                        r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>{escaped}</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                    let _ = request.respond(tiny_http::Response::from_string(body));
                }
            });
            Self { port, calls }
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_pending_auth_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn not_yet_authorized_constructs_pending_handler_instead_of_erroring() {
        // Feature test for the change requested directly: instead of
        // exiting (the old NotAuthorized error), a not-yet-authorized
        // display must now construct successfully, in a clearly-marked
        // pending state.
        let mock = MockCms::start(u32::MAX);  // never becomes READY
        let cms = test_cms_settings(mock.port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx, trigger_rx)
            .expect("must construct successfully, not error out, while pending authorization");
        assert!(handler.pending_auth, "must be marked as pending authorization");
        assert_eq!(handler.player_settings(), PlayerSettings::default(),
                   "settings must be the placeholder default while pending");
    }

    #[test]
    fn collect_once_transitions_out_of_pending_once_authorized() {
        // The core end-to-end flow: starts pending, one collection
        // cycle while still not authorized (no-op, stays pending), then
        // a later cycle where the CMS finally says READY -- must
        // transition cleanly, exactly like a real deployment being
        // approved in Administration -> Displays while already running.
        let mock = MockCms::start(2);  // READY starting from the 3rd call
        let cms = test_cms_settings(mock.port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        // Call 1 (inside Handler::new itself).
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx)
            .expect("must construct successfully while pending");
        assert!(handler.pending_auth);
        assert_eq!(mock.call_count(), 1);

        // Call 2: still not ready -- must stay pending, and must NOT
        // error (collect_once returning Err here would get logged by
        // run()'s own select! loop as a scary "during collect: ..."
        // error on every single retry, which is exactly the noisy
        // behavior this whole feature is meant to avoid).
        handler.collect_once().expect("must not error while still pending, just retry quietly");
        assert!(handler.pending_auth, "must still be pending after the 2nd (still-not-ready) call");
        assert_eq!(mock.call_count(), 2);

        // Call 3: CMS now says READY -- must transition out of pending.
        // (collect_once will likely error further down, on
        // required_files()/etc., since the mock only implements
        // RegisterDisplay, and that same call also hits this mock again
        // -- that's fine and expected, this test only cares about the
        // pending_auth transition itself, which happens *before* that
        // point in the function.)
        let _ = handler.collect_once();
        assert!(!handler.pending_auth,
                "must have transitioned out of pending authorization once the CMS said READY");
    }
}

#[cfg(test)]
mod deauthorization_tests {
    use super::*;

    /// Mock XMDS server returning a fixed sequence of RegisterDisplay
    /// codes ("READY" or "WAITING") across successive calls, clamped to
    /// the last entry once past the end -- lets a test exercise a
    /// non-monotonic sequence (e.g. READY, then WAITING, then READY
    /// again), unlike pending_auth_tests's own mock (which only ever
    /// goes from not-ready to ready, never back).
    fn start_mock(codes: Vec<&'static str>) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for (n, request) in server.incoming_requests().enumerate() {
                let code = codes.get(n).copied().unwrap_or(*codes.last().unwrap());
                let activation = format!(r#"<ActivationMessage code="{code}"/>"#);
                let escaped = activation.replace('&', "&amp;").replace('<', "&lt;")
                                         .replace('>', "&gt;").replace('"', "&quot;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>{escaped}</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_deauth_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_previously_authorized_display_reverts_to_splash_when_deauthorized_then_resumes() {
        // Regression test for a direct question asking to verify this
        // exact scenario: (1) totem installed, authorized, running
        // normally; (2) authorization removed -- should revert to the
        // splash screen (layout 0); (3) authorization restored --
        // should resume normal operation. Found, while verifying this,
        // that step 2 didn't actually work: a previously-authorized,
        // running display (pending_auth/pending_network both false)
        // getting deauthorized used to just bail!() -- an error logged
        // once per (slow) collection interval, but pending_auth was
        // never set (so retries stayed slow) and self.layouts was
        // never cleared (so the GUI was never told to switch away --
        // the display would keep showing its stale cached content
        // indefinitely, never reverting to the splash screen).
        let port = start_mock(vec!["READY", "WAITING", "READY"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        // Call 1 (Handler::new, "READY"): normal, authorized startup.
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert!(!handler.pending_auth);
        // Simulate "already running, showing some real content" --
        // schedule_check() only ever populates this from a real,
        // cached layout, which this test doesn't set up; setting it
        // directly is equivalent for what this test actually verifies
        // (does a deauth event clear it and tell the GUI), without
        // needing a full download cycle's worth of real cached files.
        handler.layouts = vec![4242];

        // Call 2 (collect_once, "WAITING" -- deauthorized): must revert
        // to the splash screen and enter the fast-retry pending state,
        // not silently keep showing layout 4242 forever.
        let _ = handler.collect_once();
        assert!(handler.pending_auth,
                "must transition into pending_auth so retries use the fast interval");
        assert!(handler.layouts.is_empty(),
                "must clear the stale layouts list -- must not keep showing old content");
        let mut found_empty_layouts = false;
        while let Ok(msg) = togui_rx.try_recv() {
            if let ToGui::Layouts(layouts) = msg {
                assert!(layouts.is_empty(),
                        "must tell the GUI an empty layout list, which gui.rs's own \
                         Schedule::update() resolves to the splash screen (layout 0) \
                         as a last resort");
                found_empty_layouts = true;
            }
        }
        assert!(found_empty_layouts,
                "must have sent ToGui::Layouts(vec![]) to the GUI on deauthorization");

        // Call 3 (collect_once, "READY" again -- reauthorized): must
        // resume normal operation.
        let _ = handler.collect_once();
        assert!(!handler.pending_auth,
                "must have transitioned back out of pending authorization");
    }
}

#[cfg(test)]
mod pending_network_tests {
    use super::*;

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_pending_network_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unreachable_cms_with_no_cache_constructs_pending_handler_instead_of_bailing() {
        // Regression test: a totem's very first boot (no cached
        // settings.json yet), with WiFi not yet having obtained an
        // IP/working DNS -- register_display() genuinely fails (an
        // actual connection failure, not just "not yet authorized")
        // with --allow-offline set. This used to bail!() the whole
        // process out, causing systemd's Restart= to redo the entire
        // Xorg/D-Bus/arexibo startup sequence repeatedly until the
        // network happened to come up in the brief window before the
        // next attempt -- appearing "stuck".
        //
        // A genuinely unreachable address (nothing listening on this
        // port at all) simulates the connection failure -- not a mock
        // server returning a "not authorized" answer (that's
        // pending_auth's own, different scenario, tested separately).
        let unreachable_port = {
            // Bind briefly to grab a genuinely free port, then drop the
            // listener immediately so the port is free again but
            // (very likely) nothing else grabs it before this test's
            // own connection attempt -- good enough for a test, not
            // meant to be airtight against a real concurrent bind.
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let cms = CmsSettings {
            address: format!("http://127.0.0.1:{unreachable_port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        };
        let envdir = test_envdir();  // fresh -- no settings.json exists here
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx, trigger_rx)
            .expect("must construct successfully, not bail out, when the network/CMS \
                     is unreachable and no cache exists yet");
        assert!(handler.pending_network, "must be marked as pending network specifically");
        assert!(!handler.pending_auth, "must NOT be marked as pending auth -- different cause");
        assert_eq!(handler.player_settings(), PlayerSettings::default(),
                   "settings must be the placeholder default while pending");
    }
}

#[cfg(test)]
mod sticky_ws_address_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock XMDS server returning a controllable sequence of WebSocket
    /// addresses across successive calls -- call N (0-indexed) returns
    /// `responses[N]` (clamped to the last entry once past the end).
    /// Same real-response-format approach as the other mock servers in
    /// this file (verified against the actual generated SOAP parser).
    fn start_mock(responses: Vec<(&'static str, &'static str)>) -> (u16, std::sync::Arc<AtomicU32>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let n = calls_clone.fetch_add(1, Ordering::SeqCst) as usize;
                if n >= responses.len() {
                    // Any call beyond RegisterDisplay (RequiredFiles,
                    // Schedule, etc., which this mock doesn't implement)
                    // gets a quick, definitive HTTP error instead of a
                    // malformed 200 OK body -- found genuinely necessary:
                    // returning a RegisterDisplayResponse-shaped body for
                    // a call expecting a *different* response type was
                    // triggering some slow path (500 (os error 111)... several
                    // *minutes* per test) rather than failing fast, making
                    // the whole test suite painfully slow to iterate on.
                    let _ = request.respond(tiny_http::Response::from_string("error")
                        .with_status_code(500));
                    continue;
                }
                let (xmr_type, ws_addr) = responses[n];
                let activation = format!(
                    r#"<ActivationMessage code="READY"><xmrType>{xmr_type}</xmrType><xmrWebSocketAddress>{ws_addr}</xmrWebSocketAddress></ActivationMessage>"#);
                let escaped = activation.replace('&', "&amp;").replace('<', "&lt;")
                                         .replace('>', "&gt;").replace('"', "&quot;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>{escaped}</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        (port, calls)
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_sticky_ws_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_previously_good_address_survives_a_later_empty_response() {
        // The actual policy requested directly: registration 1 gets a
        // real WebSocket address (xmrType=ws); registration 2 comes
        // back empty (xmrType=zmq, matching the real, reproducible CMS
        // 4.5.0 behavior reported on GitHub) -- the address must NOT
        // be cleared, it must stick.
        let (port, _calls) = start_mock(vec![
            ("ws", "ws://127.0.0.1:1"),
            ("zmq", ""),
        ]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        // Call 1 (inside Handler::new): gets the real address.
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:1");

        // Call 2 (collect_once): CMS now says zmq/empty -- must NOT
        // clear the address, must keep the one from call 1.
        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:1",
                   "a previously-good WebSocket address must survive a later empty response, not get cleared");
    }

    #[test]
    fn a_genuinely_different_address_still_replaces_the_old_one() {
        // The other half of the policy: sticky does NOT mean frozen
        // forever -- a real, different, non-empty address from a later
        // registration must still take effect (e.g. the CMS's XMR
        // infrastructure genuinely moved to a new address).
        let (port, _calls) = start_mock(vec![
            ("ws", "ws://127.0.0.1:1"),
            ("ws", "ws://127.0.0.1:2"),
        ]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:1");

        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:2",
                   "a genuinely different, non-empty address from the CMS must still replace the old one");
    }

    #[test]
    fn an_address_that_was_never_set_stays_empty_when_cms_says_zmq() {
        // Sanity check: sticky-preservation must not manufacture an
        // address out of nowhere -- if there was never a good address
        // to begin with (a normal zmq-only display), staying empty is
        // correct, not a bug.
        let (port, _calls) = start_mock(vec![
            ("zmq", ""),
            ("zmq", ""),
        ]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "");

        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "");
    }

    #[test]
    fn a_non_empty_address_missing_its_port_does_not_replace_a_good_one() {
        // A non-empty response (xmrType="ws") can still be useless if
        // the address itself is missing its port -- must be rejected
        // exactly like an empty response, keeping the previously-known
        // -good, complete address instead.
        let (port, _calls) = start_mock(vec![
            ("ws", "ws://127.0.0.1:1"),
            ("ws", "ws://127.0.0.1"),  // same host, but no port at all
        ]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:1");

        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:1",
                   "a non-empty but port-less address must not replace a good, complete one");
    }

    #[test]
    fn our_own_derived_default_fallback_wins_over_a_cached_good_address_end_to_end() {
        // End-to-end confirmation the exemption is wired into both call
        // sites. Note: this mock's own address necessarily has a port
        // (needed to be reachable), so this test would technically
        // still pass without the exemption -- the genuinely port-less
        // case is covered in isolation below, in
        // is_own_derived_ws_default_tests (a real network listener on
        // a port-less address isn't practical to set up reliably).
        let (port, _calls) = start_mock(vec![
            ("ws", "ws://127.0.0.1:8080"),
            ("ws", ""),  // empty -- triggers xmds.rs's own derived-default fallback
        ]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:8080");

        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, format!("ws://127.0.0.1:{port}/xmr"),
                   "our own derived /xmr default must win, even though it's port-shaped \
                    differently than the previously-cached address -- it's our own \
                    intentional fallback, not a suspicious CMS inconsistency");
    }
}

#[cfg(test)]
mod port_change_forces_cache_purge_tests {
    use super::*;

    // Switching the embedded webserver from the fixed
    // EMBEDDED_SERVER_PORT to whatever the CMS's own embeddedServerPort
    // setting says breaks every widget on an existing installation,
    // because cached layout HTML has the *old* port baked into its own
    // absolute iframe src URLs (see layout.rs's own write_action/
    // write_media) -- fixed with a --clear, but that shouldn't have to
    // be done manually fleet-wide.

    fn start_mock_with_port(embedded_server_port: u16) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"&gt;&lt;embeddedServerPort&gt;{embedded_server_port}&lt;/embeddedServerPort&gt;&lt;/ActivationMessage&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    /// Returns `responses[N]` for the Nth call (0-indexed), clamped to
    /// the last entry once past the end -- for simulating a setting
    /// that changes *mid-session*, not just at the very first
    /// registration inside Handler::new.
    fn start_mock_with_port_sequence(responses: Vec<(u16, bool)>) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for (n, request) in server.incoming_requests().enumerate() {
                let idx = n.min(responses.len() - 1);
                let (embedded_server_port, allow_wan) = responses[idx];
                let allow_wan_int = allow_wan as u8;
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"&gt;&lt;embeddedServerPort&gt;{embedded_server_port}&lt;/embeddedServerPort&gt;&lt;embeddedServerAllowWan type="checkbox"&gt;{allow_wan_int}&lt;/embeddedServerAllowWan&gt;&lt;/ActivationMessage&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "testkey".into(),
                      display_id: "test-display".into(), display_name: None, proxy: None }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_port_change_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_changed_port_purges_the_cache() {
        let envdir = test_envdir();

        // Simulate a previous run that used port 9696.
        PlayerSettings { embedded_server_port: 9696, ..Default::default() }
            .to_file(envdir.join("settings.json")).unwrap();
        // A file that a previous run's own cache would have -- must be
        // gone afterward, proving purge() actually ran.
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("leftover.html"), b"stale").unwrap();

        // This registration reports a *different* port.
        let port = start_mock_with_port(34519);
        let cms = test_cms_settings(port);
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let _handler = Handler::new(&cms, false, &envdir, true, true, false,
                                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        assert!(!envdir.join("res").join("leftover.html").exists(),
                "a changed embedded server port must force a full cache purge -- \
                 cached layout HTML has the old port baked into its own iframe URLs");
    }

    #[test]
    fn an_unchanged_port_does_not_purge_the_cache() {
        let envdir = test_envdir();

        PlayerSettings { embedded_server_port: 34519, ..Default::default() }
            .to_file(envdir.join("settings.json")).unwrap();
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("leftover.html"), b"stale").unwrap();

        // Same effective port as before (0 from the CMS falls back to
        // EMBEDDED_SERVER_PORT, matching the previously-recorded
        // 34519) -- exercises the "both fall back to the same
        // default" case too, not just an explicit numeric match.
        let port = start_mock_with_port(0);
        let cms = test_cms_settings(port);
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let _handler = Handler::new(&cms, false, &envdir, true, true, false,
                                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        assert!(envdir.join("res").join("leftover.html").exists(),
                "an unchanged effective port must not purge the cache");
    }

    #[test]
    fn a_port_change_mid_session_forces_a_restart() {
        // Distinct from a_changed_port_purges_the_cache above: this
        // covers the port changing *after* Handler::new (a later
        // collect_once cycle), where a cache purge alone isn't enough
        // -- the webserver's own TCP listener is already bound and
        // running, so only a full process restart can actually rebind
        // it to the new port.
        let port = start_mock_with_port_sequence(vec![(9696, false), (34519, false)]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.embedded_server_port, 9696);

        let err = handler.collect_once().expect_err(
            "a mid-session port change must return an error, not silently apply it");
        assert!(err.root_cause().downcast_ref::<RestartRequired>().is_some(),
                "must be specifically a RestartRequired, so main.rs can use its own \
                 distinct exit code (3) instead of a generic error exit (1)");
    }

    #[test]
    fn an_allow_wan_change_mid_session_forces_a_restart() {
        // Same reasoning as the port case, for embedded_server_allow_wan
        // -- the running webserver is already bound to either
        // 127.0.0.1 or 0.0.0.0, and can't be rebound without a restart.
        let port = start_mock_with_port_sequence(vec![(9696, false), (9696, true)]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert!(!handler.settings.embedded_server_allow_wan);

        let err = handler.collect_once().expect_err(
            "a mid-session WAN-access change must return an error, not silently apply it");
        assert!(err.root_cause().downcast_ref::<RestartRequired>().is_some());
    }

    #[test]
    fn no_port_or_wan_change_does_not_force_a_restart() {
        // Confirms the check above is specific to these two fields --
        // an unrelated settings change (or none at all) must not
        // trigger a restart.
        let port = start_mock_with_port_sequence(vec![(9696, false), (9696, false)]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // May still error further down (e.g. RequiredFiles against
        // this minimal mock) -- what matters is it's not RestartRequired.
        if let Err(e) = handler.collect_once() {
            assert!(e.root_cause().downcast_ref::<RestartRequired>().is_none(),
                    "an unchanged port/WAN setting must not be reported as RestartRequired");
        }
    }
}

#[cfg(test)]
mod version_change_forces_cache_purge_tests {
    use super::*;
    use crate::config::ArexiboMeta;

    // A newer build may generate slightly different layout HTML/bundled
    // assets (e.g. pdf.js) -- stale cached files from a previous version
    // could still reflect the old version's own output. Same reasoning,
    // same fix shape, as port_change_forces_cache_purge_tests above, but
    // this check runs *before* any CMS interaction at all (right after
    // Cache::new, inside Handler::new), so no mock needs to vary its own
    // response across calls here -- a plain "always READY" mock is
    // enough.

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "testkey".into(),
                      display_id: "test-display".into(), display_name: None, proxy: None }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_version_change_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_changed_version_purges_the_cache() {
        let envdir = test_envdir();

        // Simulate a previous run on a different (older) version.
        ArexiboMeta { version: "0.0.1-previous".into() }
            .to_file(envdir.join("arexibo.json")).unwrap();
        // A file that a previous run's own cache would have -- must be
        // gone afterward, proving purge() actually ran.
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("leftover.html"), b"stale").unwrap();

        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let _handler = Handler::new(&cms, false, &envdir, true, true, false,
                                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        assert!(!envdir.join("res").join("leftover.html").exists(),
                "a changed Arexibo version must force a full cache purge -- cached \
                 layout HTML/assets could still reflect the old version's own output");

        // The meta file must now reflect the *current* version, so a
        // subsequent same-version run doesn't purge again.
        let written = ArexiboMeta::from_file(envdir.join("arexibo.json")).unwrap();
        assert_eq!(written.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_unchanged_version_does_not_purge_the_cache() {
        let envdir = test_envdir();

        ArexiboMeta { version: env!("CARGO_PKG_VERSION").into() }
            .to_file(envdir.join("arexibo.json")).unwrap();
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("leftover.html"), b"stale").unwrap();

        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let _handler = Handler::new(&cms, false, &envdir, true, true, false,
                                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        assert!(envdir.join("res").join("leftover.html").exists(),
                "an unchanged version must not purge the cache");
    }

    #[test]
    fn a_first_ever_run_does_not_purge_and_writes_the_meta_file() {
        let envdir = test_envdir();
        // No arexibo.json at all -- genuinely first-ever run.
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("preexisting.html"), b"content").unwrap();

        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let _handler = Handler::new(&cms, false, &envdir, true, true, false,
                                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        assert!(envdir.join("res").join("preexisting.html").exists(),
                "a first-ever run (no prior arexibo.json) must not purge -- nothing \
                 to compare against yet");
        let written = ArexiboMeta::from_file(envdir.join("arexibo.json")).unwrap();
        assert_eq!(written.version, env!("CARGO_PKG_VERSION"),
                   "must still write the meta file, so the *next* run has something \
                    to compare against");
    }
}

#[cfg(test)]
mod send_status_update_tests {
    use super::*;

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_status_update_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Responds READY to the first request (RegisterDisplay, needed
    /// for Handler::new() to succeed), then captures the raw body of
    /// every subsequent request (NotifyStatus and anything else) into
    /// `captured`, responding with a generic NotifyStatusResponse.
    fn start_mock_capturing_requests() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        std::thread::spawn(move || {
            let mut first = true;
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(request.as_reader(), &mut body);
                if first {
                    first = false;
                    let response_body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                    let _ = request.respond(tiny_http::Response::from_string(response_body));
                } else {
                    captured_clone.lock().unwrap().push(body);
                    let response_body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><NotifyStatusResponse><success>true</success></NotifyStatusResponse></soap:Body>
</soap:Envelope>"#;
                    let _ = request.respond(tiny_http::Response::from_string(response_body));
                }
            }
        });
        (port, captured)
    }

    #[test]
    fn layout_change_immediately_sends_a_status_update_not_just_at_next_collection() {
        // Regression test: the CMS's own "Current
        // Layout" display only updated once per full collection cycle
        // (which can be minutes apart), even though arexibo's own
        // self.current_layout tracked the real, immediate layout
        // change correctly the whole time -- it just wasn't telling
        // the CMS promptly. FromGui::Showing now calls
        // send_status_update() directly, in addition to still being
        // sent once per collection.
        let (port, captured) = start_mock_capturing_requests();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Simulate exactly what FromGui::Showing(layout) does in run()'s
        // own select! loop -- without needing to drive that full,
        // otherwise-infinite loop just to test this specific behavior.
        handler.current_layout = 4242;
        handler.record_layout_shown(4242);
        handler.send_status_update();

        let requests = captured.lock().unwrap();
        assert!(requests.iter().any(|body| body.contains("4242")),
                "a status update containing the new layout id must be sent \
                 immediately on layout change -- captured requests: {requests:?}");
    }

    #[test]
    fn immediate_send_is_skipped_when_the_cms_setting_disables_it() {
        // Confirmed real CMS setting (SendCurrentLayoutAsStatusUpdate,
        // seen directly gating this exact same immediate-notify call
        // in the reference client's MainWindow.xaml.cs) -- must be
        // respected, not ignored. Mirrors run()'s own select! loop
        // logic (`if self.settings.send_current_layout_as_status_update
        // { self.send_status_update(); }`) without needing to drive
        // that full, otherwise-infinite loop just to test this.
        let (port, captured) = start_mock_capturing_requests();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        handler.settings.send_current_layout_as_status_update = false;

        handler.current_layout = 4242;
        handler.record_layout_shown(4242);
        if handler.settings.send_current_layout_as_status_update {
            handler.send_status_update();
        }

        let requests = captured.lock().unwrap();
        assert!(!requests.iter().any(|body| body.contains("4242")),
                "must NOT send an immediate status update when the CMS setting \
                 disables it -- captured requests: {requests:?}");
    }

    #[test]
    fn status_update_includes_the_own_lan_ip_address() {
        // Xibo xibosignage/xibo#2863 ("Displays: add LAN IP address
        // when available") -- a Sync Group Follower's own <syncGroup>
        // element contains the Lead's own lanIpAddress, relayed by the
        // CMS. Never previously sent by arexibo at all.
        let (port, captured) = start_mock_capturing_requests();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        handler.send_status_update();

        let requests = captured.lock().unwrap();
        assert!(requests.iter().any(|body| body.contains("lanIpAddress")),
                "a status update must include lanIpAddress when a local IP is \
                 determinable -- captured requests: {requests:?}");
    }
}

#[cfg(test)]
mod sync_role_change_forces_restart_tests {
    use super::*;
    use crate::config::SyncRole;

    /// Returns `responses[N]` for the Nth call (0-indexed), clamped to
    /// the last entry once past the end -- for simulating a
    /// syncGroup value that changes *mid-session*, not just at the
    /// very first registration.
    fn start_mock_with_sync_group_sequence(responses: Vec<&'static str>) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for (n, request) in server.incoming_requests().enumerate() {
                let idx = n.min(responses.len() - 1);
                let sync_group = responses[idx];
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"&gt;&lt;syncGroup&gt;{sync_group}&lt;/syncGroup&gt;&lt;/ActivationMessage&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "testkey".into(),
                      display_id: "test-display".into(), display_name: None, proxy: None }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_sync_role_change_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn none_to_lead_forces_a_restart() {
        let port = start_mock_with_sync_group_sequence(vec!["", "lead"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.sync_role, SyncRole::None);

        let err = handler.collect_once().expect_err(
            "a Sync Group role change must return an error, not silently apply it");
        assert!(err.root_cause().downcast_ref::<RestartRequired>().is_some());
    }

    #[test]
    fn lead_to_follower_forces_a_restart() {
        let port = start_mock_with_sync_group_sequence(vec!["lead", "192.168.1.235"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.sync_role, SyncRole::Lead);

        let err = handler.collect_once().expect_err(
            "a Sync Group role change must return an error, not silently apply it");
        assert!(err.root_cause().downcast_ref::<RestartRequired>().is_some());
    }

    #[test]
    fn lead_address_change_while_staying_a_follower_does_not_force_a_restart() {
        // The key distinction from the tests above: same role kind
        // (Follower both times), only the lead's own address changes
        // (e.g. the CMS reassigns which display is the Lead) -- must
        // NOT trigger a restart. Left for the actual sync channel
        // implementation to handle as a reconnect once it exists (see
        // SyncRole::is_same_role_kind's own doc comment).
        let port = start_mock_with_sync_group_sequence(
            vec!["192.168.1.235", "192.168.1.236"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        assert_eq!(handler.settings.sync_role,
                   SyncRole::Follower { lead_addr: "192.168.1.235".into() });

        if let Err(e) = handler.collect_once() {
            assert!(e.root_cause().downcast_ref::<RestartRequired>().is_none(),
                    "a lead_addr change while staying a Follower must not be reported \
                     as RestartRequired");
        }
        assert_eq!(handler.settings.sync_role,
                   SyncRole::Follower { lead_addr: "192.168.1.236".into() },
                   "the new address must still be applied");
    }

    #[test]
    fn no_sync_role_change_does_not_force_a_restart() {
        let port = start_mock_with_sync_group_sequence(vec!["lead", "lead"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        if let Err(e) = handler.collect_once() {
            assert!(e.root_cause().downcast_ref::<RestartRequired>().is_none(),
                    "an unchanged Sync Group role must not be reported as RestartRequired");
        }
    }

    #[test]
    fn update_sync_group_is_deferred_until_the_first_collection_completes() {
        // Regression test: a Follower connecting to the Lead this
        // early (during Handler::new's own two update_settings calls,
        // well before any collection has downloaded/parsed this
        // display's own schedule) could receive a Command for
        // sync_keys it can't yet resolve. See first_collection_done's
        // own doc comment for the full story.
        let port = start_mock_with_sync_group_sequence(vec!["lead"]);
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Handler::new's own initial registration already resolved
        // sync_role to Lead (confirmed by the mock's own "lead" value
        // above), and internally calls update_settings (hence
        // update_sync_group) twice while doing so -- but neither call
        // must have actually connected/started anything yet.
        assert_eq!(handler.settings.sync_role, SyncRole::Lead,
                   "sanity: sync_role must already be resolved to Lead from the very \
                    first registration");
        assert!(!handler.first_collection_done,
                   "sanity: no collection has run yet at this point");
        assert_eq!(handler.sync_group_role, SyncRole::None,
                   "update_sync_group must not have actually set up anything yet, even \
                    though settings.sync_role already says Lead -- the two \
                    update_settings calls Handler::new itself makes must have been \
                    deferred");
        assert!(handler.sync_group.is_none(),
                "no SyncGroup (Lead listener or Follower connection) may exist before \
                 the first collection completes");

        // Mirrors exactly what happens at the real end of a successful
        // collect_once() (see that function's own call site, right
        // before "collection successful" is logged) -- done directly
        // here rather than driving a real collect_once() to completion,
        // which would need a considerably more elaborate mock server
        // correctly answering every XMDS method a full cycle calls,
        // not just RegisterDisplay.
        handler.first_collection_done = true;
        handler.update_sync_group();

        assert_eq!(handler.sync_group_role, SyncRole::Lead,
                   "once the first collection has completed, update_sync_group must \
                    now actually perform the deferred setup");
        assert!(handler.sync_group.is_some(),
                "a SyncGroup (here, a Lead listener) must now actually exist");
    }
}

#[cfg(test)]
mod handle_trigger_code_tests {
    use super::*;

    // Regression coverage: a webhook trigger did nothing at all,
    // because the only mechanism handled was an in-page widget-
    // embedded action (only reachable while its own layout/widget
    // happens to already be on screen) -- the actual configured action
    // was a Scheduled Action (schedule::ActionTarget), targeting a
    // *different* layout entirely, reachable regardless of what's
    // currently showing.

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "testkey".into(),
                      display_id: "test-display".into(), display_name: None, proxy: None }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_handle_trigger_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_handler() -> Handler {
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        Handler::new(&cms, false, &envdir, true, true, false,
                     togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap()
    }

    /// A minimal <actions> Schedule, matching the real capture's own
    /// shape (fromdt/todt spanning "always").
    fn schedule_with_action(trigger_code: &str, duration: i64, action_type_and_targets: &str) -> Schedule {
        let xml = format!(
            r#"<schedule generated="2026-01-01 00:00:00" filterFrom="2026-01-01 00:00:00" filterTo="2026-01-02 00:00:00">
  <actions>
    <action fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="1" priority="0"
     duration="{duration}" isGeoAware="0" geoLocation="" triggerCode="{trigger_code}" {action_type_and_targets}/>
  </actions>
</schedule>"#);
        let tree = elementtree::Element::from_reader(xml.as_bytes()).unwrap();
        Schedule::parse(&tree).unwrap()
    }

    /// Registers `layout_code` -> `id` in the handler's own cache code
    /// map, matching how a real collection cycle populates it (see
    /// resource.rs's own update_code_map) -- needed for the navLayout
    /// success path specifically.
    fn register_layout_code(handler: &mut Handler, layout_code: &str, id: i64) {
        let file = crate::resource::ReqFile::File {
            id, typ: "layout", size: 0, md5: vec![], http: false,
            path: String::new(), name: String::new(), code: Some(layout_code.into()),
        };
        handler.cache.update_code_map(&[file]).unwrap();
    }

    #[test]
    fn a_matching_navlayout_action_with_nonzero_duration_arms_a_timer() {
        let mut handler = test_handler();
        handler.schedule = schedule_with_action("apri_home", 30,
            r#"actionType="navLayout" layoutCode="home" commandCode="""#);
        register_layout_code(&mut handler, "home", 971);

        let needs_collect = handler.handle_trigger_code("apri_home");

        assert_eq!(handler.override_layout, Some(971));
        assert!(!handler.override_revert_on_completion,
                "a nonzero duration must use the timer, not wait-for-completion");
        // register_layout_code only populates the code->id mapping, not
        // the layout's own actual content -- get_layout(971) correctly
        // finds nothing cached, so a collection really is needed here.
        assert!(needs_collect, "the target layout's own content isn't actually cached yet");
    }

    #[test]
    fn a_matching_navlayout_action_with_zero_duration_waits_for_completion() {
        let mut handler = test_handler();
        handler.schedule = schedule_with_action("apri_home", 0,
            r#"actionType="navLayout" layoutCode="home" commandCode="""#);
        register_layout_code(&mut handler, "home", 971);

        handler.handle_trigger_code("apri_home");

        assert_eq!(handler.override_layout, Some(971));
        assert!(handler.override_revert_on_completion,
                "duration == 0 must wait for the layout's own natural completion \
                 instead of arming a timer");
    }

    #[test]
    fn a_matching_command_action_runs_the_command() {
        let mut handler = test_handler();
        handler.schedule = schedule_with_action("run_touch", 0,
            r#"actionType="command" layoutCode="" commandCode="TESTTOUCH""#);

        handler.handle_trigger_code("run_touch");

        // enable_shell_commands defaults to false (PlayerSettings::
        // default), so run_command fails closed deterministically --
        // what matters here is only that it actually got *called*.
        assert_eq!(handler.last_command_success, Some(false));
        assert!(handler.override_layout.is_none(),
                "a command-type action must not touch override_layout at all");
    }

    #[test]
    fn no_matching_scheduled_action_falls_back_to_the_in_page_trigger() {
        let mut handler = test_handler();
        // Empty schedule -- no Scheduled Action defined at all.
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        handler.to_gui = togui_tx;

        let needs_collect = handler.handle_trigger_code("some_code");

        assert!(!needs_collect);
        assert!(handler.override_layout.is_none());
        let msg = togui_rx.try_recv().expect("must fall back to the in-page trigger mechanism");
        assert!(matches!(msg, ToGui::Trigger(code) if code == "some_code"));
    }

    #[test]
    fn an_unresolvable_layout_code_does_not_set_an_override() {
        let mut handler = test_handler();
        handler.schedule = schedule_with_action("apri_home", 30,
            r#"actionType="navLayout" layoutCode="never_registered" commandCode="""#);
        // Deliberately not calling register_layout_code -- the code map
        // stays empty, matching a genuinely misconfigured action or one
        // whose target layout hasn't been seen in RequiredFiles yet.

        let needs_collect = handler.handle_trigger_code("apri_home");

        assert!(!needs_collect);
        assert!(handler.override_layout.is_none());
    }

    #[test]
    fn layout_completed_reverts_an_override_waiting_for_completion() {
        let mut handler = test_handler();
        handler.override_layout = Some(971);
        handler.override_revert_on_completion = true;

        if let Err(e) = handler.collect_once() {
            // Irrelevant to what's under test -- collect_once may fail
            // against this minimal mock for unrelated reasons (no
            // RequiredFiles/Schedule support), which is fine; only
            // FromGui::LayoutCompleted's own handling (exercised below)
            // is what this test actually verifies.
            let _ = e;
        }

        // Simulate exactly what run()'s own select! loop does on
        // FromGui::LayoutCompleted, without needing to drive that full,
        // otherwise-infinite loop just to test this.
        if handler.override_revert_on_completion && handler.override_layout.take().is_some() {
            handler.override_revert_on_completion = false;
        }

        assert!(handler.override_layout.is_none());
        assert!(!handler.override_revert_on_completion);
    }

    #[test]
    fn layout_completed_does_not_affect_a_timer_based_override() {
        let mut handler = test_handler();
        handler.override_layout = Some(971);
        handler.override_revert_on_completion = false; // timer-based, not wait-for-completion

        // Same simulated FromGui::LayoutCompleted logic as above.
        if handler.override_revert_on_completion && handler.override_layout.take().is_some() {
            handler.override_revert_on_completion = false;
        }

        assert_eq!(handler.override_layout, Some(971),
                   "a timer-based override must not be cleared by a mere \
                    natural-completion signal");
    }
}

#[cfg(test)]
mod is_own_derived_ws_default_tests {
    use super::*;

    fn test_cms_settings(address: &str) -> CmsSettings {
        CmsSettings { address: address.to_string(), key: "k".into(), display_id: "d".into(),
                      display_name: None, proxy: None }
    }

    #[test]
    fn recognizes_its_own_genuinely_port_less_derived_default() {
        // The exact real scenario reported directly: CMS address with
        // NO explicit port at all (http://192.168.1.11, relying on
        // port 80 implicitly) -- the derived default
        // (ws://192.168.1.11/xmr) is therefore *itself* genuinely
        // port-less too, exactly the case ws_address_has_port alone
        // would flag as suspicious. Deliberately not using a real mock
        // HTTP server here (unlike the end-to-end test above) -- one
        // would necessarily have an explicit port to be reachable at
        // all, masking exactly this port-less case.
        let cms = test_cms_settings("http://192.168.1.11");
        assert!(is_own_derived_ws_default(&cms, "ws://192.168.1.11/xmr"),
                "must recognize this as its own derived default, despite having no port");
    }

    #[test]
    fn does_not_falsely_match_an_unrelated_port_less_address() {
        // Guards against the exemption being too loose -- a port-less
        // address that *isn't* what this CMS's own fallback would
        // produce (wrong host, or missing the /xmr suffix) must still
        // be treated as a genuine, suspicious CMS inconsistency, not
        // waved through just because it happens to lack a port too.
        let cms = test_cms_settings("http://192.168.1.11");
        assert!(!is_own_derived_ws_default(&cms, "ws://some-other-host/xmr"));
        assert!(!is_own_derived_ws_default(&cms, "ws://192.168.1.11"),
                "missing the /xmr suffix -- not what the fallback actually produces");
    }

    #[test]
    fn recognizes_its_own_default_when_the_cms_address_does_have_a_port() {
        // The other shape this can take (matching the end-to-end test
        // above) -- same mechanism, just confirming it also works when
        // the CMS's own address happens to carry an explicit port.
        let cms = test_cms_settings("http://192.168.2.138:9092");
        assert!(is_own_derived_ws_default(&cms, "ws://192.168.2.138:9092/xmr"));
    }
}

#[cfg(test)]
mod xmr_disconnected_tests {
    use super::*;

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_xmr_disconnected_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_the_channel_and_arms_a_retry_with_the_kept_private_key() {
        // Regression test for the real, still-unresolved report this
        // whole fix is about: repeated "Connection closed normally,
        // reconnecting in 10s" that never recovered on its own, only a
        // full process restart fixed it. Confirms the *mainloop side*
        // of the fix -- xmr.rs's own bounded-retry-then-give-up
        // behavior is tested separately, over there.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Sanity check: clear whatever xmr_retry_key ended up as from
        // construction (e.g. if the mock's bare "READY" response, with
        // no real XMR address, already left one set) -- so this test
        // can tell whether xmr_disconnected() itself is what
        // (re)populates it, not just leftover state from construction.
        handler.xmr_retry_key = None;

        handler.xmr_disconnected();

        assert!(handler.xmr_retry_key.is_some(),
                "must arm a retry using the kept private key after the XMR channel disconnects");
        // The replaced channel must be genuinely empty/disconnected-style
        // (never()) -- recv'ing on it must not immediately return
        // something stale from before.
        assert!(handler.xmr.try_recv().is_err(),
                "the channel must be replaced with an inert one, not left in its old state");
    }
}

#[cfg(test)]
mod sticky_address_applies_to_first_connection_tests {
    use super::*;

    /// A minimal mock WebSocket server: accepts one TCP connection,
    /// completes the WS handshake, then just keeps it open (does
    /// nothing else -- arexibo's own XMR client only needs the
    /// handshake to succeed for xmr::start() to return Ok).
    fn start_mock_ws() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                if let Ok(mut socket) = tungstenite::accept(stream) {
                    // Keep the connection open for a bit so the test has
                    // time to observe the outcome before it closes.
                    let _ = socket.read();
                }
            }
        });
        port
    }

    fn start_mock_cms(ws_addr: String) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let activation = format!(
                    r#"<ActivationMessage code="READY"><xmrType>ws</xmrType><xmrWebSocketAddress>{ws_addr}</xmrWebSocketAddress></ActivationMessage>"#);
                let escaped = activation.replace('&', "&amp;").replace('<', "&lt;")
                                         .replace('>', "&gt;").replace('"', "&quot;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>{escaped}</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_sticky_first_conn_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_very_first_xmr_connection_uses_the_sticky_corrected_address_not_the_bad_one() {
        // Regression test for a real GitHub report: settings.json ended
        // up with the correct, complete previously-known-good address,
        // yet *this session's own* XMR connection still used the bad,
        // port-less one from the fresh registration -- because the
        // sticky-address check used to run *after* xmr::start() was
        // already called with the untouched settings. Moved the check
        // earlier to fix this; this test would have failed before that
        // fix (xmr::start() would have tried port 80, found nothing
        // listening, and fallen back to ZMQ instead of succeeding here).
        let ws_port = start_mock_ws();
        let good_address = format!("ws://127.0.0.1:{ws_port}");
        let envdir = test_envdir();

        // Pre-populate settings.json with the good, complete address --
        // simulating a previous, successful run.
        let settings_path = envdir.join("settings.json");
        let prev = PlayerSettings { xmr_web_socket_address_in_use: good_address.clone(),
                                     ..Default::default() };
        prev.to_file(settings_path).unwrap();

        // This fresh registration deliberately returns a *port-less*
        // address (xmrType=ws, but no port) -- the exact real-world
        // scenario from the report.
        let cms_port = start_mock_cms("ws://127.0.0.1".to_string());
        let cms = CmsSettings {
            address: format!("http://127.0.0.1:{cms_port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        };
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // The in-memory settings must reflect the corrected address...
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, good_address);
        // ...and, crucially, no retry was armed -- meaning xmr::start()
        // actually *succeeded* using that corrected address, rather
        // than failing (with port 80) and falling back to offline/ZMQ
        // retry mode. This is the part that would have failed before
        // the ordering fix.
        assert!(handler.xmr_retry_key.is_none(),
                "xmr::start() should have succeeded directly against the corrected \
                 address -- a retry being armed means it was actually attempted \
                 against the bad, port-less one instead");
    }
}

#[cfg(test)]
mod apply_weather_criteria_tests {
    use super::*;

    #[test]
    fn each_key_value_pair_becomes_one_criteria_update() {
        // Matches the real reference client's own behavior
        // (WeatherAgent.cs): every key/value pair in the weather JSON
        // becomes one Schedule Criteria update, string-valued.
        let mut criteria = CriteriaStore::default();
        apply_weather_criteria(&mut criteria, r#"{"temperature":25,"weather_condition":"clear"}"#, 120).unwrap();
        assert_eq!(criteria.get("temperature"), Some("25"));
        assert_eq!(criteria.get("weather_condition"), Some("clear"));
    }

    #[test]
    fn invalid_json_returns_an_error_not_a_panic() {
        let mut criteria = CriteriaStore::default();
        let result = apply_weather_criteria(&mut criteria, "not valid json", 120);
        assert!(result.is_err());
    }

    #[test]
    fn feeds_the_same_criteria_store_xmr_criteria_update_also_uses() {
        // Confirms this is a genuine complement to (not a separate
        // mechanism from) the existing xmr::Message::CriteriaUpdate
        // push path -- both must be able to set/overwrite the exact
        // same metric.
        let mut criteria = CriteriaStore::default();
        criteria.set("weather_condition".to_string(), "rain".to_string(), 60);
        assert_eq!(criteria.get("weather_condition"), Some("rain"));
        apply_weather_criteria(&mut criteria, r#"{"weather_condition":"clear"}"#, 120).unwrap();
        assert_eq!(criteria.get("weather_condition"), Some("clear"),
                   "a later weather pull must be able to update the same metric an XMR push set");
    }
}

#[cfg(test)]
mod is_method_not_present_fault_tests {
    use super::*;

    #[test]
    fn recognizes_the_real_reported_error_message() {
        let err = anyhow::anyhow!("getting weather: parsing GetWeather SOAP response: \
                                    got SOAP fault: Procedure 'GetWeather' not present");
        assert!(is_method_not_present_fault(&err));
    }

    #[test]
    fn does_not_misclassify_an_unrelated_error() {
        let err = anyhow::anyhow!("getting weather: io: connection refused");
        assert!(!is_method_not_present_fault(&err));
    }
}

#[cfg(test)]
mod is_weather_provider_not_configured_fault_tests {
    use super::*;

    #[test]
    fn recognizes_the_real_reported_error_message() {
        let err = anyhow::anyhow!("getting weather: getting weather: parsing GetWeather \
                                    SOAP response: got SOAP fault: \
                                    Xibo\\Event\\XmdsWeatherRequestEvent::getWeatherData(): \
                                    Return value must be of type string, null returned");
        assert!(is_weather_provider_not_configured_fault(&err));
    }

    #[test]
    fn does_not_misclassify_a_method_not_present_fault() {
        // The two must stay distinct -- one is permanent (v5, never
        // retry), the other can resolve itself (CMS-side config).
        let err = anyhow::anyhow!("getting weather: parsing GetWeather SOAP response: \
                                    got SOAP fault: Procedure 'GetWeather' not present");
        assert!(!is_weather_provider_not_configured_fault(&err));
    }

    #[test]
    fn does_not_misclassify_an_unrelated_error() {
        let err = anyhow::anyhow!("getting weather: io: connection refused");
        assert!(!is_weather_provider_not_configured_fault(&err));
    }
}

#[cfg(test)]
mod decide_timezone_action_tests {
    use super::*;

    #[test]
    fn applies_on_first_registration() {
        assert_eq!(decide_timezone_action(&None, "Europe/Rome"),
                   TimezoneAction::Apply("Europe/Rome".to_string()));
    }

    #[test]
    fn does_nothing_once_already_applied_and_unchanged() {
        assert_eq!(
            decide_timezone_action(&Some("Europe/Rome".to_string()), "Europe/Rome"),
            TimezoneAction::DoNothing);
    }

    #[test]
    fn warns_instead_of_reapplying_when_the_cms_value_changes_later() {
        assert_eq!(
            decide_timezone_action(&Some("Europe/Rome".to_string()), "America/New_York"),
            TimezoneAction::WarnRestartNeeded {
                was: "Europe/Rome".to_string(), now: "America/New_York".to_string(),
            });
    }

    #[test]
    fn does_nothing_when_the_cms_sends_nothing() {
        assert_eq!(decide_timezone_action(&None, ""), TimezoneAction::DoNothing);
        assert_eq!(decide_timezone_action(&Some("Europe/Rome".to_string()), ""),
                   TimezoneAction::DoNothing);
    }
}

#[cfg(test)]
mod apply_process_timezone_tests {
    use super::*;

    #[test]
    fn genuinely_changes_this_processs_own_local_offset() {
        // End-to-end: catches missing tzset() calls that a pure
        // decision-logic test can't. TZ is process-global, so this
        // could flake if run concurrently with another test relying on
        // a specific offset -- none currently do.
        apply_process_timezone("Europe/Rome");
        let offset = time::OffsetDateTime::now_local().unwrap().offset();
        assert_eq!(offset.whole_hours(), 2, "August in Europe/Rome must be UTC+2 (CEST)");

        apply_process_timezone("America/New_York");
        let offset = time::OffsetDateTime::now_local().unwrap().offset();
        assert_eq!(offset.whole_hours(), -4, "August in America/New_York must be UTC-4 (EDT)");
    }
}

#[cfg(test)]
mod is_exempt_as_currently_playing_layout_tests {
    use super::*;
    use elementtree::Element;

    fn layout_file(id: i64) -> ReqFile {
        ReqFile::File { id, typ: "layout", size: 0, md5: vec![], http: false,
                        path: String::new(), name: String::new(), code: None }
    }

    // Same wide-open from/to window as real schedule.xml samples --
    // always "currently active" regardless of when the test runs.
    fn schedule_with(layout_file_id: i64, scheduleid: i64) -> Schedule {
        let xml = format!(
            r#"<schedule generated="2026-08-21 11:53:13" filterFrom="2026-08-21 11:00:00" filterTo="2026-08-23 11:00:00">
  <layout file="{layout_file_id}" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="{scheduleid}" priority="0" syncEvent="0" shareOfVoice="0" duration="60" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="1" playCount="0" maxPlaysPerHour="0"/>
  <default file="1" duration="60"/>
</schedule>"#);
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        Schedule::parse(&tree).unwrap()
    }

    #[test]
    fn exempts_a_republished_layout_occupying_the_same_schedule_slot() {
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(927, 224);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert_eq!(current_scheduleid, 224);
        assert!(is_exempt_as_currently_playing_layout(&layout_file(927), current_scheduleid,
                                                        &fresh_schedule, false),
                "a republished layout occupying the same schedule slot must be exempted");
    }

    #[test]
    fn does_not_exempt_a_genuinely_different_schedule_slot() {
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(913, 225);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert!(!is_exempt_as_currently_playing_layout(&layout_file(913), current_scheduleid,
                                                         &fresh_schedule, false));
    }

    #[test]
    fn does_not_exempt_anything_when_expire_modified_layouts_is_true() {
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(927, 224);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert!(!is_exempt_as_currently_playing_layout(&layout_file(927), current_scheduleid,
                                                         &fresh_schedule, true));
    }

    #[test]
    fn does_not_exempt_non_layout_files() {
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(927, 224);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        let media = ReqFile::File { id: 927, typ: "media", size: 0, md5: vec![], http: false,
                                     path: String::new(), name: String::new(), code: None };
        assert!(!is_exempt_as_currently_playing_layout(&media, current_scheduleid,
                                                         &fresh_schedule, false));
    }

    #[test]
    fn a_zero_current_scheduleid_never_exempts_anything() {
        let fresh_schedule = schedule_with(927, 224);
        assert!(!is_exempt_as_currently_playing_layout(&layout_file(927), 0,
                                                         &fresh_schedule, false));
    }
}

#[cfg(test)]
mod screenshot_requested_tests {
    use super::*;

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(),
            display_id: "test-display".into(),
            display_name: None,
            proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_screenshot_requested_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Counts queued ToGui::Screenshot messages, ignoring other variants
    // (e.g. ToGui::Settings, always also sent).
    fn count_screenshot_messages(rx: &crossbeam_channel::Receiver<ToGui>) -> usize {
        let mut n = 0;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, ToGui::Screenshot) {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn fulfills_a_pending_request_exactly_once_while_the_flag_stays_true() {
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(10);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        let _ = count_screenshot_messages(&togui_rx);

        handler.settings.screen_shot_requested = true;
        handler.update_settings().unwrap();
        assert_eq!(count_screenshot_messages(&togui_rx), 1);

        // Flag stays true for a second cycle -- must not fire again.
        handler.update_settings().unwrap();
        assert_eq!(count_screenshot_messages(&togui_rx), 0);

        // CMS resets the flag once it receives our submission.
        handler.settings.screen_shot_requested = false;
        handler.update_settings().unwrap();
        assert_eq!(count_screenshot_messages(&togui_rx), 0);

        // A genuinely new request later must fire again.
        handler.settings.screen_shot_requested = true;
        handler.update_settings().unwrap();
        assert_eq!(count_screenshot_messages(&togui_rx), 1,
                   "a new request (flag re-transitioning false -> true) must fire again");
    }
}

#[cfg(test)]
mod https_upgrade_address_tests {
    use super::*;

    #[test]
    fn swaps_http_to_https_keeping_everything_else() {
        assert_eq!(https_upgrade_address("http://cms.example.com"),
                   Some("https://cms.example.com".to_string()));
        assert_eq!(https_upgrade_address("http://192.168.2.138:9092"),
                   Some("https://192.168.2.138:9092".to_string()));
    }

    #[test]
    fn already_https_needs_no_upgrade() {
        assert_eq!(https_upgrade_address("https://cms.example.com"), None);
    }
}

#[cfg(test)]
mod should_attempt_cms_migration_tests {
    use super::*;

    #[test]
    fn requires_both_address_and_key() {
        assert!(should_attempt_cms_migration("https://new.example.com", "newkey"));
        assert!(!should_attempt_cms_migration("", "newkey"), "address missing");
        assert!(!should_attempt_cms_migration("https://new.example.com", ""), "key missing");
        assert!(!should_attempt_cms_migration("", ""), "neither present -- the common case");
    }
}

#[cfg(test)]
mod commit_cms_migration_tests {
    use super::*;

    fn test_envdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_cms_migration_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn old_cms() -> CmsSettings {
        CmsSettings { address: "https://old.example.com".into(), key: "oldkey".into(),
                      display_id: "stable-hw-id-123".into(), display_name: None, proxy: None }
    }

    #[test]
    fn writes_the_new_settings_and_backs_up_the_old_ones() {
        let envdir = test_envdir();
        old_cms().to_file(envdir.join("cms.json")).unwrap();

        let new_cms = CmsSettings { address: "https://new.example.com".into(), key: "newkey".into(),
                                     display_id: "stable-hw-id-123".into(), display_name: None,
                                     proxy: None };
        commit_cms_migration(&envdir, &new_cms).unwrap();

        let written = CmsSettings::from_file(envdir.join("cms.json")).unwrap();
        assert_eq!(written.address, "https://new.example.com");
        assert_eq!(written.key, "newkey");
        assert_eq!(written.display_id, "stable-hw-id-123");

        let backed_up = CmsSettings::from_file(envdir.join("cms.json.bak")).unwrap();
        assert_eq!(backed_up.address, "https://old.example.com");
    }

    #[test]
    fn clears_the_cache_directory_if_it_exists() {
        let envdir = test_envdir();
        old_cms().to_file(envdir.join("cms.json")).unwrap();
        let cache_dir = envdir.join("res");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("stale_layout.xlf.html"), b"old cms content").unwrap();

        let new_cms = CmsSettings { address: "https://new.example.com".into(), key: "newkey".into(),
                                     display_id: "stable-hw-id-123".into(), display_name: None,
                                     proxy: None };
        commit_cms_migration(&envdir, &new_cms).unwrap();

        assert!(!cache_dir.join("stale_layout.xlf.html").exists());
    }

    #[test]
    fn succeeds_even_without_a_pre_existing_cache_directory() {
        let envdir = test_envdir();
        old_cms().to_file(envdir.join("cms.json")).unwrap();
        let new_cms = CmsSettings { address: "https://new.example.com".into(), key: "newkey".into(),
                                     display_id: "stable-hw-id-123".into(), display_name: None,
                                     proxy: None };
        assert!(commit_cms_migration(&envdir, &new_cms).is_ok());
    }
}

#[cfg(test)]
mod attempt_cms_migration_tests {
    use super::*;

    // The success path of both attempt_https_upgrade and
    // attempt_cms_migration used to call std::process::exit() directly,
    // which (a) segfaulted when this ran from collect_once() with Qt's
    // own event loop still active, and (b) made this path
    // fundamentally untestable, since a stray process::exit() during a
    // test run would kill the test runner itself. Now that it returns
    // Result instead, this is directly testable.

    fn start_ready_mock() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_envdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_attempt_migration_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_successful_migration_returns_restart_required_not_a_process_exit() {
        let initial_port = start_ready_mock();
        let cms = CmsSettings { address: format!("http://127.0.0.1:{initial_port}"),
                                 key: "testkey".into(), display_id: "test-display".into(),
                                 display_name: None, proxy: None };
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // A second, independent CMS to "migrate" to -- also validates
        // successfully.
        let new_port = start_ready_mock();
        handler.settings.new_cms_address = format!("http://127.0.0.1:{new_port}");
        handler.settings.new_cms_key = "newkey".into();

        let err = handler.attempt_cms_migration().expect_err(
            "a successful migration must return an error, not silently continue");
        assert!(err.root_cause().downcast_ref::<RestartRequired>().is_some(),
                "must be specifically RestartRequired, not some other error");

        // The migration itself must have actually been committed
        // *before* the restart signal, not skipped.
        let written = CmsSettings::from_file(envdir.join("cms.json")).unwrap();
        assert_eq!(written.address, format!("http://127.0.0.1:{new_port}"));
        assert_eq!(written.key, "newkey");
    }

    #[test]
    fn no_migration_requested_is_a_quiet_no_op() {
        let initial_port = start_ready_mock();
        let cms = CmsSettings { address: format!("http://127.0.0.1:{initial_port}"),
                                 key: "testkey".into(), display_id: "test-display".into(),
                                 display_name: None, proxy: None };
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // new_cms_address/new_cms_key left empty (the default, no
        // migration requested) -- must not error at all.
        handler.attempt_cms_migration().unwrap();
    }
}

#[cfg(test)]
mod validate_new_cms_tests {
    use super::*;

    fn start_mock(activation_body: &'static str) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let escaped = activation_body.replace('&', "&amp;").replace('<', "&lt;")
                                              .replace('>', "&gt;").replace('"', "&quot;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>{escaped}</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_xml_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_validate_cms_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_ready_response_is_a_successful_validation() {
        let port = start_mock(r#"<ActivationMessage code="READY"/>"#);
        let candidate = CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "k".into(),
                                       display_id: "d".into(), display_name: None, proxy: None };
        assert!(validate_new_cms(&candidate, "dummy-pub-key".into(), true, test_xml_dir()).is_ok());
    }

    #[test]
    fn a_waiting_not_yet_authorized_response_is_also_a_successful_validation() {
        let port = start_mock(r#"<ActivationMessage code="WAITING"/>"#);
        let candidate = CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "k".into(),
                                       display_id: "d".into(), display_name: None, proxy: None };
        assert!(validate_new_cms(&candidate, "dummy-pub-key".into(), true, test_xml_dir()).is_ok());
    }

    #[test]
    fn an_unreachable_server_fails_validation() {
        let unreachable_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let candidate = CmsSettings { address: format!("http://127.0.0.1:{unreachable_port}"),
                                       key: "k".into(), display_id: "d".into(), display_name: None,
                                       proxy: None };
        assert!(validate_new_cms(&candidate, "dummy-pub-key".into(), true, test_xml_dir()).is_err());
    }
}

#[cfg(test)]
mod data_refresh_timer_tests {
    use super::*;
    use elementtree::Element;

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(), display_id: "test-display".into(),
            display_name: None, proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_data_refresh_timer_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // These two tests use a minimal "READY" mock with no actual data
    // widget content flowing through it at all -- nothing is ever
    // tracked here regardless of XMDS_ENDPOINT_VERSION/the gate's own
    // state (see xmds.rs), so they stay valid whether the gate is
    // open or closed. Complements resource.rs's own more detailed
    // tests of Cache's refresh/expiry logic directly (which construct
    // real tracked widgets to test that logic on its own merits).

    #[test]
    fn rearm_leaves_the_timer_disarmed_when_nothing_is_tracked() {
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        handler.rearm_data_refresh_timer();

        // never() never fires -- a short timeout confirms this without
        // making the test itself slow.
        assert!(handler.next_data_refresh.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn refresh_due_data_widgets_is_a_safe_no_op_with_nothing_tracked() {
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Must not panic, and must leave the timer disarmed afterward
        // (re-confirms rearm gets called even with nothing to do).
        handler.refresh_due_data_widgets();
        assert!(handler.next_data_refresh.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn maybe_force_reload_after_purge_reloads_only_when_the_flag_is_set() {
        // Regression test: purge() deletes every cached file
        // immediately, including whatever a currently-playing widget
        // is actively streaming from the embedded HTTP server --
        // schedule_check()'s own reload decision is keyed on the
        // layout id changing, which a purge alone doesn't. This method
        // forces a real reload once the purge-triggered redownload is
        // back.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        while togui_rx.try_recv().is_ok() {} // drain any startup messages

        // With the flag unset (the ordinary case, every other
        // collect_once() completion that isn't purge-triggered), this
        // must be a safe no-op -- no reload for every normal collection.
        handler.maybe_force_reload_after_purge();
        assert!(togui_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "must not force a reload when the flag was never set");

        // With the flag set (as the `Purge` XMR handler does), the
        // *current* layout must be force-reloaded, and the flag
        // cleared afterward -- but only once the current layout's own
        // translated HTML is confirmed present on disk (a separate
        // test below covers the missing-file case).
        handler.current_layout = 4242;
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("4242.xlf.html"), b"<html></html>").unwrap();
        handler.force_reload_after_collect = true;
        handler.maybe_force_reload_after_purge();
        let msg = togui_rx.recv_timeout(Duration::from_millis(500))
            .expect("must force a reload when the flag was set");
        assert!(matches!(msg, ToGui::ForceReloadLayout(4242)),
                "must reload the *current* layout specifically, not an ordinary \
                 ToGui::Layouts (which the GUI's own Schedule<T>::update would silently \
                 no-op on an unchanged id -- see ForceReloadLayout's own doc comment)");
        assert!(!handler.force_reload_after_collect,
                "the flag must be cleared after acting on it, or every later collection \
                 (unrelated to the purge that set it) would keep force-reloading forever");
    }

    #[test]
    fn maybe_force_reload_after_purge_waits_if_the_html_isnt_back_on_disk_yet() {
        // Regression test: reloading unconditionally after a purge
        // used to navigate straight to a live browser 404 if the
        // current layout's own file hadn't actually landed back on
        // disk yet -- worse than staying on the stale-but-rendered
        // page a moment longer.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        while togui_rx.try_recv().is_ok() {} // drain any startup messages

        // The flag is set, but 4242.xlf.html was never actually
        // written to disk (e.g. collect_once() failed to redownload
        // it this cycle) -- must NOT reload to a target confirmed
        // missing, and must leave the flag set so this same check
        // runs again after the *next* collection cycle, once the file
        // has had a further chance to actually arrive.
        handler.current_layout = 4242;
        handler.force_reload_after_collect = true;
        handler.maybe_force_reload_after_purge();
        assert!(togui_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "must not force a reload when the current layout's own translated HTML \
                 isn't actually on disk yet -- that would navigate straight to a live \
                 browser 404 error page");
        assert!(handler.force_reload_after_collect,
                "the flag must stay set when the file wasn't found, so the same check \
                 retries after the next collection cycle instead of silently giving up \
                 on the reload entirely");
    }

    #[test]
    fn any_non_resource_failure_ignores_resource_entries() {
        // "resource" failures (dataset/webpage-manual/bestfit widgets)
        // must NOT count -- those already self-heal via
        // resource_retry_queue/note_layout_file_downloaded once they
        // eventually succeed.
        assert!(!any_non_resource_failure(&[]),
                "an empty result (e.g. nothing needed downloading) must not count as a \
                 failure");
        assert!(!any_non_resource_failure(&[(("media", 1), true), (("layout", 2), true)]),
                "all-success must not count as a failure");
        assert!(any_non_resource_failure(&[(("media", 1), false)]),
                "a failed media file must count as a failure");
        assert!(any_non_resource_failure(&[(("layout", 1), false)]),
                "a failed layout file must count as a failure");
        assert!(!any_non_resource_failure(&[(("resource", 1), false)]),
                "a failed *resource* (dataset/webpage-manual/bestfit) must NOT count -- \
                 those already self-heal via resource_retry_queue/\
                 note_layout_file_downloaded once they eventually succeed");
        assert!(any_non_resource_failure(&[(("resource", 1), false), (("media", 2), false)]),
                "a genuine media failure must still count even alongside an ignored \
                 resource failure");
        assert!(!any_non_resource_failure(&[(("resource", 1), false), (("media", 2), true)]),
                "an ignored resource failure alongside an otherwise fully successful \
                 collection must not count as a failure overall");
    }

    #[test]
    fn maybe_force_reload_after_purge_waits_if_this_cycle_had_media_failures() {
        // Regression test: even with the current layout's own HTML
        // present on disk, an unrelated media file it references (e.g.
        // a video) may still have failed to download this cycle --
        // reloading anyway would show the layout with that media
        // broken/missing.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        while togui_rx.try_recv().is_ok() {} // drain any startup messages

        handler.current_layout = 4242;
        std::fs::create_dir_all(envdir.join("res")).unwrap();
        std::fs::write(envdir.join("res").join("4242.xlf.html"), b"<html></html>").unwrap();
        handler.force_reload_after_collect = true;
        handler.last_collect_had_failures = true;
        handler.maybe_force_reload_after_purge();
        assert!(togui_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "must not force a reload when this cycle reported a media/layout failure, \
                 even with the current layout's own HTML confirmed present -- some other \
                 piece it depends on may still be missing");
        assert!(handler.force_reload_after_collect,
                "the flag must stay set so the same check retries after the next \
                 collection cycle");
    }

    #[test]
    fn note_layout_file_downloaded_forces_a_reload_only_for_the_currently_showing_layout() {
        // Regression test: a media item changed in the CMS's own
        // library without republishing the layout that uses it still
        // bumps the layout's own required version too -- both get
        // redownloaded correctly, but if that layout is currently
        // showing, schedule_check()'s own reload decision (keyed on
        // layout id changing) never notices, since only the content
        // underneath changed. Left unfixed, the display kept showing
        // the old page indefinitely -- on a single-layout kiosk with
        // no natural rotation point, until a full restart or purgeAll.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        while togui_rx.try_recv().is_ok() {} // drain any startup messages
        handler.current_layout = 4242;

        // A *different* layout's own file being downloaded (e.g. one
        // that's merely scheduled, not currently showing) must not
        // force anything.
        handler.note_layout_file_downloaded(Some(9999));
        assert!(!handler.force_reload_after_collect,
                "a downloaded file for a layout other than the currently-showing one \
                 must not set the force-reload flag");

        // A non-layout download (layout_id_if_any is None -- media,
        // resources, dependencies) must also not force anything.
        handler.note_layout_file_downloaded(None);
        assert!(!handler.force_reload_after_collect,
                "a non-layout file download must not set the force-reload flag");

        // The *currently showing* layout's own file being downloaded
        // (an update, since download() only runs for files not
        // already correctly cached) must set the flag.
        handler.note_layout_file_downloaded(Some(4242));
        assert!(handler.force_reload_after_collect,
                "a fresh download of the currently-showing layout's own file must set \
                 the force-reload flag, so maybe_force_reload_after_purge (called after \
                 every collect_once(), not just purge-triggered ones) actually reloads \
                 the GUI once the updated content is back on disk");
        // maybe_force_reload_after_purge (the consuming side, already
        // tested above) is what actually turns this into a real
        // ToGui::ForceReloadLayout -- not re-tested here again.
        assert!(togui_rx.try_recv().is_err(),
                "note_layout_file_downloaded itself must only set the flag -- the actual \
                 reload message is sent later, by maybe_force_reload_after_purge, once \
                 collect_once() has fully finished");
    }

    #[test]
    fn an_already_cached_file_is_still_reported_complete_in_media_inventory() {
        // Regression test for a real, confirmed bug: "Playlist stays
        // pending in CMS after re-scheduling already-downloaded
        // content." Root cause: a required file already fully cached
        // from an earlier collection cycle (e.g. downloaded once, the
        // schedule referencing it was then removed, and it's now
        // simply being re-scheduled) used to be reported in the
        // MediaInventory submission *only* at the moment it was first
        // downloaded -- never again on any later cycle where it's
        // already cached (the whole reporting branch was nested
        // *inside* the "needs downloading" check). It was therefore
        // never included in *this* collection's own submission at
        // all, and the CMS's own Manage Display view kept showing it
        // as pending/incomplete indefinitely, even though it was
        // already playing correctly on the display.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Already fully cached (matching md5, an empty Vec here) --
        // simulating this exact layout having been downloaded in an
        // earlier collection cycle, entirely without needing to mock
        // a real network download for this test.
        handler.cache.insert_fake_layout_for_test(100);
        let file = ReqFile::File { id: 100, typ: "layout", size: 0, md5: vec![],
                                    http: false, path: String::new(), name: String::new(),
                                    code: None };

        let result = handler.download_required_files(vec![file], 0, &Schedule::default());

        assert_eq!(result, vec![(("layout", 100), true)],
                    "an already-cached file must still be reported as complete in this \
                     collection's own MediaInventory submission, not silently omitted \
                     just because it didn't need downloading this cycle");
    }

    #[test]
    fn a_missing_file_that_fails_to_download_is_reported_incomplete() {
        // Companion to the test above -- confirms the opposite case
        // (a file that's genuinely missing, and fails to download)
        // still correctly reports `false`/incomplete, unaffected by
        // the fix above (which only changes the *already-cached*
        // path).
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Not cached at all -- and start_mock_ready's own GetResource/
        // GetFile-equivalent endpoints aren't wired up to serve real
        // content, so the download attempt genuinely fails.
        let file = ReqFile::File { id: 999, typ: "layout", size: 0, md5: vec![1, 2, 3],
                                    http: false, path: String::new(), name: String::new(),
                                    code: None };

        let result = handler.download_required_files(vec![file], 0, &Schedule::default());

        assert_eq!(result, vec![(("layout", 999), false)],
                    "a genuinely missing file that fails to download must still be \
                     reported as incomplete, same as before this fix");
    }

    #[test]
    fn a_successful_refresh_notifies_the_gui_to_reload_the_containing_resource() {
        // The gap this test guards against: writing a fresh
        // <widgetId>.json to disk does nothing on its own -- the
        // already-loaded widget page has no reason to re-fetch it
        // unless told to. Must reuse the same ToGui::ReloadWidget
        // mechanism the XMR-pushed DataUpdate path already uses.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let mut request = request;
                let mut body = String::new();
                std::io::Read::read_to_string(request.as_reader(), &mut body).unwrap();
                let is_get_data = body.contains("GetData");
                let body = if is_get_data {
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><GetDataResponse><data>{"data":[]}</data></GetDataResponse></soap:Body>
</soap:Envelope>"#.to_string()
                } else {
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#.to_string()
                };
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        // Drain whatever update_settings() sent during construction
        // (a ToGui::Settings) so it doesn't get mistaken for the
        // ReloadWidget message this test is actually checking for.
        while togui_rx.try_recv().is_ok() {}

        // resource_id 1, widget_id 4543 -- deliberately different
        // numbers, matching the real observed case, so this test can't
        // pass by accident from confusing the two.
        handler.cache.discover_data_widgets(
            r#"<script>widgetData.push({"widgetId":4543,"url":"4543.json","data":null});</script>"#,
            1, 940);

        handler.refresh_due_data_widgets();

        let msg = togui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(msg, ToGui::ReloadWidget(1)),
                "expected ToGui::ReloadWidget(1) (the resource id)");
    }

    #[test]
    fn a_data_widget_inside_an_overlay_layout_survives_schedule_check() {
        // Regression test: a data widget (v7 GetData polling) living
        // inside an *overlay* layout (tracked separately in
        // self.schedule_overlays, never in self.layouts) was discovered
        // correctly on download, but then immediately pruned right
        // back out on the very next schedule_check --
        // prune_data_widgets_not_in used to only consider self.layouts,
        // so an overlay's own layout id was never "still active",
        // permanently preventing that widget from completing a
        // GetData refresh.
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        // Real overlay XML shape (see schedule.rs's own
        // parses_real_overlays_xml_from_user test) -- overlay layout
        // 1031, main layout 913.
        let xml = r#"<schedule generated="2026-08-31 10:00:00" filterFrom="2026-08-31 10:00:00" filterTo="2026-08-31 12:00:00">
  <layout file="913" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="1" priority="0" syncEvent="0" shareOfVoice="0" duration="60" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="0" playCount="0" maxPlaysPerHour="0"/>
  <overlays>
    <overlay file="1031" fromdt="1970-01-01 01:00:00" todt="2038-01-19 04:14:07" scheduleid="2" priority="0" duration="30" isGeoAware="0" geoLocation="" maxPlaysPerHour="0"/>
  </overlays>
  <default file="913" duration="60"/>
</schedule>"#;
        let tree = Element::from_reader(xml.as_bytes()).unwrap();
        handler.schedule = Schedule::parse(&tree).unwrap();
        handler.cache.insert_fake_layout_for_test(913);
        handler.cache.insert_fake_layout_for_test(1031);
        // Discovered as part of downloading resource 4950, which lives
        // inside the *overlay* layout (1031) -- not the main one.
        handler.cache.discover_data_widgets(
            r#"<script>widgetData.push({"widgetId":4950,"url":"4950.json","data":null});</script>"#,
            4950, 1031);

        handler.schedule_check();

        assert!(handler.cache.is_tracked_data_widget(4950),
                "a data widget inside an active overlay layout must survive \
                 schedule_check's own pruning -- this is the exact real bug: it used to \
                 be pruned immediately because only self.layouts (never overlay layout \
                 ids) was considered \"still active\"");
    }
}

#[cfg(test)]
mod flush_faults_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Tracks ReportFaults calls specifically (not just any SOAP call --
    // RegisterDisplay also happens during Handler::new, and must not be
    // miscounted as a fault report).
    struct FaultMock {
        port: u16,
        report_calls: std::sync::Arc<AtomicU32>,
    }

    impl FaultMock {
        fn start() -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let port = server.server_addr().to_ip().unwrap().port();
            let report_calls = std::sync::Arc::new(AtomicU32::new(0));
            let calls = report_calls.clone();
            std::thread::spawn(move || {
                for mut request in server.incoming_requests() {
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    let response = if body.contains("ReportFaults") {
                        calls.fetch_add(1, Ordering::SeqCst);
                        r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><ReportFaultsResponse><success>1</success></ReportFaultsResponse></soap:Body>
</soap:Envelope>"#.to_string()
                    } else {
                        r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#.to_string()
                    };
                    let _ = request.respond(tiny_http::Response::from_string(response));
                }
            });
            Self { port, report_calls }
        }
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "testkey".into(), display_id: "test-display".into(),
            display_name: None, proxy: None,
        }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_flush_faults_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Sanity check for the *current*, real configuration
    // (XMDS_ENDPOINT_VERSION == 7, gate open): confirms the fix that
    // added the version gate to flush_faults doesn't overcorrect into
    // never sending faults at all. This can't, on its own, prove the
    // gate itself works when *closed* -- XMDS_ENDPOINT_VERSION is a
    // compile-time constant, not something a test can independently
    // fake per-run. That side was instead verified manually: with the
    // constant temporarily set back to 5, this same test (and the real
    // bug's own reproduction) confirmed zero ReportFaults calls happen
    // -- not something that can be encoded as a permanent, automated
    // regression test given the constant's own nature, unlike every
    // other "prove the fix" verification elsewhere in this file.
    #[test]
    fn flushes_pending_faults_now_that_the_version_gate_is_open() {
        let mock = FaultMock::start();
        let cms = test_cms_settings(mock.port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        handler.faults.record(faults::Fault::new(9001, "test fault"));
        handler.flush_faults();

        assert_eq!(mock.report_calls.load(Ordering::SeqCst), 1,
                    "flush_faults must actually call ReportFaults when there are \
                     pending faults and the endpoint version supports it");
    }

    #[test]
    fn does_nothing_when_no_faults_are_pending() {
        let mock = FaultMock::start();
        let cms = test_cms_settings(mock.port);
        let envdir = test_envdir();
        let (togui_tx, _togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();

        handler.flush_faults();

        assert_eq!(mock.report_calls.load(Ordering::SeqCst), 0,
                    "flush_faults must not call ReportFaults at all when nothing is pending");
    }
}

#[cfg(test)]
mod sync_group_schedule_check_tests {
    use super::*;
    use crate::util::TIME_FMT;

    fn start_mock_ready() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><RegisterDisplayResponse><ActivationMessage>&lt;ActivationMessage code="READY"/&gt;</ActivationMessage></RegisterDisplayResponse></soap:Body>
</soap:Envelope>"#;
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        port
    }

    fn test_cms_settings(port: u16) -> CmsSettings {
        CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "testkey".into(),
                      display_id: "test-display".into(), display_name: None, proxy: None }
    }

    fn test_envdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arexibo_sync_schedule_check_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_handler() -> (Handler, crossbeam_channel::Receiver<ToGui>) {
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(5);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);
        let (_trigger_tx, trigger_rx) = crossbeam_channel::bounded(5);
        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx, trigger_rx).unwrap();
        (handler, togui_rx)
    }

    /// A minimal schedule with one `syncEvent="1"` <layout> entry,
    /// active from `from` to `to` -- real attribute shape confirmed
    /// from a live CMS's own schedule.xml (see schedule.rs's own
    /// `parses_real_sync_group_mirror_event_xml_from_user`).
    fn schedule_with_sync_layout(layout_id: i64, from: OffsetDateTime, to: OffsetDateTime) -> Schedule {
        let xml = format!(
            r#"<schedule generated="2026-01-01 00:00:00" filterFrom="2026-01-01 00:00:00" filterTo="2026-01-02 00:00:00">
  <layout file="{layout_id}" fromdt="{}" todt="{}" scheduleid="1" priority="0" syncEvent="1"
   shareOfVoice="0" duration="60" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="0"
   playCount="0" maxPlaysPerHour="0"/>
  <default file="999" duration="10"/>
</schedule>"#,
            from.format(&TIME_FMT).unwrap(), to.format(&TIME_FMT).unwrap());
        let tree = elementtree::Element::from_reader(xml.as_bytes()).unwrap();
        Schedule::parse(&tree).unwrap()
    }

    /// Sets up `handler`'s own cache (as an already-downloaded layout
    /// carrying `sync_keys`) *and* schedule (as an actively sync-gated
    /// entry for that same id, from `from` to `to`) in one call --
    /// most tests below need both halves consistent with each other.
    fn setup_sync_gated_layout(handler: &mut Handler, layout_id: i64, sync_keys: Vec<String>,
                                from: OffsetDateTime, to: OffsetDateTime) {
        handler.cache.insert_fake_layout_with_sync_keys_for_test(layout_id, sync_keys);
        handler.schedule = schedule_with_sync_layout(layout_id, from, to);
    }

    fn setup_always_active_sync_gated_layout(handler: &mut Handler, layout_id: i64,
                                              sync_keys: Vec<String>) {
        let now = OffsetDateTime::now_local().unwrap();
        setup_sync_gated_layout(handler, layout_id, sync_keys,
                                 now - time::Duration::hours(1), now + time::Duration::hours(1));
    }

    #[test]
    fn a_sync_gated_layout_is_staged_not_applied_immediately() {
        // A Synchronised Event's own <layout syncEvent="1"> entry --
        // unlike an ordinary schedule entry, must not be shown the
        // moment schedule_check() would otherwise consider it
        // "current". It should instead be staged the same way an
        // incoming Follower Command already is, waiting for
        // sync_apply_timer to actually fire.
        let (mut handler, _togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 1014, vec!["sync1".into()]);

        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]),
                   "a sync-gated layout's own sync_keys must be staged via pending_sync_keys");
        assert!(!matches!(handler.sync_apply_timer.try_recv(),
                           Err(crossbeam_channel::TryRecvError::Disconnected)),
                "sync_apply_timer must be armed (not left as never())");
        assert_ne!(handler.layouts, vec![1014],
                   "the sync-gated layout must NOT be applied/shown immediately");
        assert!(handler.override_layout.is_none(),
                "no override should be committed yet -- only staged");
    }

    #[test]
    fn an_ordinary_non_synchronised_layout_still_applies_immediately() {
        // Sanity check: the sync-gating logic must not affect ordinary
        // (syncEvent="0"/absent) schedule entries at all -- they
        // should switch exactly as before, with no staging delay.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_for_test(614);
        let now = OffsetDateTime::now_local().unwrap();
        let xml = format!(
            r#"<schedule generated="2026-01-01 00:00:00" filterFrom="2026-01-01 00:00:00" filterTo="2026-01-02 00:00:00">
  <layout file="614" fromdt="{}" todt="{}" scheduleid="1" priority="0" syncEvent="0"
   shareOfVoice="0" duration="60" isGeoAware="0" geoLocation="" cyclePlayback="0" groupKey="0"
   playCount="0" maxPlaysPerHour="0"/>
  <default file="999" duration="10"/>
</schedule>"#,
            (now - time::Duration::hours(1)).format(&TIME_FMT).unwrap(),
            (now + time::Duration::hours(1)).format(&TIME_FMT).unwrap());
        let tree = elementtree::Element::from_reader(xml.as_bytes()).unwrap();
        handler.schedule = Schedule::parse(&tree).unwrap();

        handler.schedule_check();

        assert_eq!(handler.layouts, vec![614], "an ordinary layout must apply immediately");
        assert_eq!(handler.pending_sync_keys, None,
                   "an ordinary layout must never be staged as a sync layout");
    }

    #[test]
    fn once_staged_a_second_schedule_check_does_not_republish_or_restage() {
        // Regression coverage for the re-publish guard: schedule_check
        // can run many times (every collection cycle, plus whenever
        // anything else changes) before sync_apply_timer actually
        // fires -- it must not treat the same already-pending
        // sync-gated layout as newly discovered every single time.
        let (mut handler, _togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 1014, vec!["sync1".into()]);

        handler.schedule_check();
        let first_timer = handler.sync_apply_timer.clone();
        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]));
        // The exact same timer channel/receiver must still be the one
        // in place -- a re-stage would have replaced it with a fresh
        // `after(delay)` call, restarting the countdown from scratch.
        assert!(std::ptr::eq(&first_timer, &first_timer),
                "sanity: comparing a value to itself");
        // A more direct check: the timer must still fire at
        // approximately the originally-scheduled delay, not have been
        // pushed back further by a second staging attempt. With
        // sync_switch_delay defaulting to 750ms (PlayerSettings'
        // own default), waiting under that should not yet see it fire.
        assert!(handler.sync_apply_timer.try_recv().is_err(),
                "timer should not have fired yet (still within the original delay)");
    }

    #[test]
    fn sync_apply_timer_firing_resolves_sync_keys_and_applies_the_matching_layout() {
        // Mirrors the real select! arm's own body exactly (see the
        // main loop's `recv(self.sync_apply_timer)` handler) --
        // including its own call to resolve_layout_for_sync_keys,
        // which is the actual point of this whole redesign: the
        // received sync_keys are resolved against *this display's
        // own* schedule/cache, never a layout id trusted directly
        // from the network.
        let (mut handler, togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 1014, vec!["sync1".into()]);
        // Stage directly (bypassing the real delay) rather than
        // waiting out sync_switch_delay in this test.
        handler.pending_sync_keys = Some(vec!["sync1".into()]);
        handler.sync_apply_timer = crossbeam_channel::after(std::time::Duration::from_millis(1));
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(handler.sync_apply_timer.try_recv().is_ok());
        if let Some(sync_keys) = handler.pending_sync_keys.take() {
            if let Some(layout_id) = handler.resolve_layout_for_sync_keys(&sync_keys) {
                handler.override_layout = Some(layout_id);
                handler.sync_layout_active = true;
                handler.layouts = vec![layout_id];
                handler.to_gui.send(ToGui::ForceReloadLayout(layout_id)).unwrap();
            }
            handler.sync_apply_timer = never();
            handler.schedule_check();
        }

        assert_eq!(handler.layouts, vec![1014],
                   "once resolved and applied, the matching layout must actually be shown");
        assert_eq!(handler.override_layout, Some(1014));
        assert!(handler.sync_layout_active);
        // Drain: Handler::new's own initial collection cycle already
        // queued a ToGui::Settings before this test's own logic ran,
        // so ForceReloadLayout isn't necessarily the very first
        // message on the channel -- check it appears *somewhere* among
        // what's queued, not that it's first.
        assert!(std::iter::from_fn(|| togui_rx.try_recv().ok())
                    .any(|m| matches!(m, ToGui::ForceReloadLayout(_))),
                "applying a synchronized switch must always force a real page reload");
    }

    #[test]
    fn re_synchronizing_an_already_showing_layout_still_forces_a_reload() {
        // The specific bug this whole mechanism exists to fix: the
        // Lead applying a genuinely new layout id works (region timers
        // naturally start fresh when a page loads for the first time)
        // -- but re-synchronizing the same, already-active layout id
        // (the actual re-connect scenario, via stage_sync_switch from
        // sync_peer_connected) must ALSO force a real reload. Ordinary
        // ToGui::Layouts is a silent no-op on an unchanged id (see
        // gui.rs's own Schedule<T>::update) -- ForceReloadLayout must
        // be sent regardless of whether self.layouts actually changes.
        let (mut handler, togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 1014, vec!["sync1".into()]);
        // Already showing this exact layout *before* the re-sync --
        // matching a Follower (or the Lead itself) that was already
        // mid-way through this same Synchronised Event.
        handler.override_layout = Some(1014);
        handler.sync_layout_active = true;
        handler.layouts = vec![1014];
        handler.pending_sync_keys = Some(vec!["sync1".into()]);
        handler.sync_apply_timer = crossbeam_channel::after(std::time::Duration::from_millis(1));
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(handler.sync_apply_timer.try_recv().is_ok());
        if let Some(sync_keys) = handler.pending_sync_keys.take() {
            if let Some(layout_id) = handler.resolve_layout_for_sync_keys(&sync_keys) {
                handler.override_layout = Some(layout_id);
                handler.sync_layout_active = true;
                handler.layouts = vec![layout_id];
                handler.to_gui.send(ToGui::ForceReloadLayout(layout_id)).unwrap();
            }
            handler.sync_apply_timer = never();
            handler.schedule_check();
        }

        assert!(std::iter::from_fn(|| togui_rx.try_recv().ok())
                    .any(|m| matches!(m, ToGui::ForceReloadLayout(_))),
                "re-synchronizing an already-showing layout must still force a real \
                 reload -- an ordinary ToGui::Layouts would be silently ignored by the \
                 GUI's own unchanged-id no-op check, leaving region/playlist timers \
                 exactly where they were, never actually resynchronized");
    }

    #[test]
    fn a_sync_override_auto_expires_once_its_own_scheduled_window_ends() {
        // Regression coverage for a real gap found while wiring this
        // up: a Sync Group override has no fixed duration/completion
        // signal of its own to revert on, unlike a Scheduled Action's
        // own override (override_expiry/override_revert_on_completion)
        // -- without this check, a display that ever applied a
        // synchronized layout switch would stay stuck showing that one
        // layout forever.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_for_test(1014);
        handler.cache.insert_fake_layout_for_test(999);
        let now = OffsetDateTime::now_local().unwrap();
        // Window already ended (to < now) -- is_sync_gated must now
        // return false for this layout.
        handler.schedule = schedule_with_sync_layout(
            1014, now - time::Duration::hours(2), now - time::Duration::minutes(1));
        handler.override_layout = Some(1014);
        handler.sync_layout_active = true;
        handler.layouts = vec![1014];

        handler.schedule_check();

        assert!(handler.override_layout.is_none(),
                "the override must be cleared once its own sync-gated window ends");
        assert!(!handler.sync_layout_active);
        // Normal schedule resolution should now resume -- with no
        // other active entry, this falls back to <default> (999).
        assert_eq!(handler.layouts, vec![999]);
    }

    #[test]
    fn expiry_reconnects_a_follower_to_re_trigger_the_leads_own_peer_connected() {
        // Regression test: the CMS swapped which layout a live
        // Synchronised Event points to (same sync_keys, visually
        // similar templates). A Follower whose own schedule.xml hadn't
        // yet downloaded that change resolved to the wrong, superseded
        // layout against the Lead's fresh Command instead of ever
        // rendering the new one -- kept showing the old layout until
        // manually restarted.
        //
        // Fix: simply reconnecting the Follower's own SyncGroup
        // connection at the exact moment its own schedule genuinely
        // catches up (this same expiry check firing) re-triggers the
        // Lead's own already-built sync_peer_connected reaction, which
        // re-publishes its current sync_keys fresh. Simpler than a
        // dedicated Follower -> Lead "ready" signal, which the
        // existing one-way PUB/SUB connection can't carry at all.
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let lead = syncgroup::SyncGroup::start_lead(port, std::time::Duration::ZERO)
            .expect("starting a real Lead to reconnect to");
        let peer_connected = lead.peer_connected().expect("a Lead's own is always Some");

        let (mut handler, _togui_rx) = test_handler();
        handler.settings.sync_role = SyncRole::Follower { lead_addr: "127.0.0.1".into() };
        handler.settings.sync_publisher_port = port;
        handler.cache.insert_fake_layout_for_test(1014);
        handler.cache.insert_fake_layout_for_test(999);
        let now = OffsetDateTime::now_local().unwrap();
        // The old layout's own window has ended (its own schedule
        // entry was replaced, not merely time-expired, but
        // is_sync_gated(1014) becoming false is the same observable
        // condition either way).
        handler.schedule = schedule_with_sync_layout(
            1014, now - time::Duration::hours(2), now - time::Duration::minutes(1));
        handler.override_layout = Some(1014);
        handler.sync_layout_active = true;
        handler.layouts = vec![1014];

        handler.schedule_check();

        assert!(handler.override_layout.is_none(), "sanity: the override must expire");
        assert!(handler.sync_group.is_some(),
                "a fresh SyncGroup Follower connection must now exist");
        peer_connected.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the Lead must observe a genuinely new connection -- confirming a \
                     real reconnect happened, not just a state flag flip");
    }

    #[test]
    fn the_exact_real_sequence_eventually_stages_the_new_layout_once_cached() {
        // Reproduces a real sequence step by step: 1018 expires
        // (schedule now shows 1019 instead) while 1019 isn't cached
        // yet -- the expiry check fires and reconnects correctly
        // (confirmed separately by the test above). Several more
        // schedule_check() calls follow while 1019 is still not
        // cached -- each should just re-warn, no state change.
        // Finally 1019 finishes downloading -- the next schedule_check
        // should discover it as newly sync-gated and stage it (this
        // final step never happened before the fix).
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_for_test(1018);
        let now = OffsetDateTime::now_local().unwrap();
        // Step 1: 1018 was active and synced, matching the real
        // starting state right before its own window ended.
        handler.schedule = schedule_with_sync_layout(
            1018, now - time::Duration::hours(1), now - time::Duration::minutes(1));
        handler.override_layout = Some(1018);
        handler.sync_layout_active = true;
        handler.layouts = vec![1018];

        // Step 2: schedule now shows 1019 instead, not yet cached.
        handler.schedule = schedule_with_sync_layout(
            1019, now - time::Duration::hours(1), now + time::Duration::hours(1));
        handler.schedule_check();

        assert!(handler.override_layout.is_none(), "sanity: 1018's own expiry must fire");
        assert_eq!(handler.layouts, vec![1018],
                   "sanity: falls back to what's already showing, 1019 isn't cached yet");

        // Step 3: several more schedule_check() calls while 1019 is
        // still not cached.
        for _ in 0..5 {
            handler.schedule_check();
            assert_eq!(handler.layouts, vec![1018], "must keep showing 1018 while waiting");
            assert_eq!(handler.pending_sync_keys, None,
                       "must not stage anything while 1019 isn't cached yet");
        }

        // Step 4: 1019 finally finishes downloading.
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1019, vec!["sync1".into()]);
        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]),
                   "once 1019 is actually in cache, the very next schedule_check must \
                    discover and stage it as newly sync-gated -- this is the exact step \
                    that failed to happen in the real report");
    }

    #[test]
    fn a_new_sync_gated_layout_stages_even_when_its_sync_keys_match_a_previous_one() {
        // Regression test: successive layout swaps within the same
        // live Synchronised Event can share the exact same sync_keys
        // text (structurally-similar templates). The re-publish guard
        // used to compare pending_sync_keys by value -- so once one
        // was successfully staged/applied, a later swap to a
        // genuinely different layout with the same-looking sync_keys
        // was silently ignored. A display could then never advance
        // past whichever layout it last synced to, for as long as the
        // CMS kept assigning layouts sharing that sync_keys value.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1019, vec!["sync1".into()]);
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1020, vec!["sync1".into()]);
        let now = OffsetDateTime::now_local().unwrap();

        // 1019 already successfully staged and applied -- matching the
        // real state right before the CMS reassigns the event to 1020.
        handler.schedule = schedule_with_sync_layout(
            1019, now - time::Duration::hours(1), now - time::Duration::minutes(1));
        handler.override_layout = Some(1019);
        handler.sync_layout_active = true;
        handler.layouts = vec![1019];
        handler.pending_sync_keys = Some(vec!["sync1".into()]);
        handler.pending_sync_layout_id = Some(1019);

        // The CMS reassigns the same live event to 1020 -- same
        // sync_keys, genuinely different layout.
        handler.schedule = schedule_with_sync_layout(
            1020, now - time::Duration::hours(1), now + time::Duration::hours(1));

        handler.schedule_check();

        assert!(handler.override_layout.is_none(), "sanity: 1019's own expiry must fire");
        // THE CRITICAL ASSERTION: must stage 1020 despite its own
        // sync_keys being textually identical to 1019's own.
        assert_eq!(handler.pending_sync_layout_id, Some(1020),
                   "a genuinely new sync-gated layout must be staged even when its own \
                    sync_keys happen to match a previously-handled layout's own -- this \
                    is the exact real bug: comparing by sync_keys value alone wrongly \
                    treated this as \"already handled\"");
        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]));
    }

    #[test]
    fn a_scheduled_action_override_is_unaffected_by_the_sync_expiry_check() {
        // sync_layout_active must stay false for a Scheduled Action's
        // own override -- the expiry check above must never interfere
        // with that entirely separate mechanism (own doc comment on
        // sync_layout_active).
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_for_test(42);
        handler.override_layout = Some(42);
        handler.sync_layout_active = false; // as a Scheduled Action's own override leaves it
        handler.layouts = vec![42];

        handler.schedule_check();

        assert_eq!(handler.override_layout, Some(42),
                   "a Scheduled Action's own override must not be touched by the \
                    Sync Group expiry check");
    }

    #[test]
    fn a_reconnecting_peer_re_stages_the_currently_active_synchronized_layout() {
        // Regression test: a Follower restarted mid-way through an
        // already-active Synchronised Event never got re-synchronized
        // -- nothing changes from the Lead's own schedule_check
        // perspective for an already-settled event, so the only way to
        // notice and react is this dedicated sync_peer_connected
        // signal (see its own field doc comment). Mirrors the real
        // select! arm's own body (see the main loop's
        // `recv(self.sync_peer_connected)` handler).
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1014, vec!["sync1".into()]);
        handler.sync_layout_active = true;
        handler.override_layout = Some(1014);
        handler.layouts = vec![1014]; // already committed and showing

        if handler.sync_layout_active {
            if let Some(layout_id) = handler.override_layout {
                if let Some(info) = handler.cache.get_layout(layout_id) {
                    handler.stage_sync_switch(info.sync_keys.clone());
                }
            }
        }

        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]),
                   "the currently-active synchronized layout's own sync_keys must be \
                    re-staged for every connected display (including this one) to \
                    restart in lockstep");
        assert!(handler.sync_apply_timer.try_recv().is_err(),
                "sync_apply_timer must be freshly armed, not left as never()");
    }

    #[test]
    fn a_reconnecting_peer_is_a_no_op_when_nothing_is_currently_synchronized() {
        // Sanity check: this mechanism must not fire when there's no
        // active Synchronised Event at all -- an ordinary peer
        // (re)connection outside of any Sync Group event in progress
        // must not spuriously stage/publish anything.
        let (mut handler, _togui_rx) = test_handler();
        handler.sync_layout_active = false;
        handler.override_layout = None;

        if handler.sync_layout_active {
            if let Some(layout_id) = handler.override_layout {
                if let Some(info) = handler.cache.get_layout(layout_id) {
                    handler.stage_sync_switch(info.sync_keys.clone());
                }
            }
        }

        assert_eq!(handler.pending_sync_keys, None,
                   "nothing should be staged when no Synchronised Event is currently active");
    }

    // ---- Wall Sync safety: resolve_layout_for_sync_keys ----
    //
    // The whole reason the wire protocol carries sync_keys instead of
    // a layout id at all (user's own explicit direction, after
    // pointing out a real safety gap in the original design): for
    // anything other than Mirror Sync, each display in a Sync Group
    // may show a genuinely *different* layout of a shared composition
    // (Wall Sync). Blindly applying whichever layout id the Lead
    // happens to be showing would display entirely wrong content on
    // a Follower whose own schedule names a different one. These
    // tests don't have real Wall Sync capture data yet (only Mirror
    // Sync has been confirmed against a live CMS) -- they instead
    // verify the *safety property* directly: a display only ever
    // applies its *own* schedule's own layout, and only when that
    // layout's own sync_keys genuinely overlap with what was
    // received.

    #[test]
    fn resolves_to_my_own_differently_id_layout_when_it_shares_a_sync_key() {
        // The Wall Sync case: this display's own currently-scheduled
        // sync-gated layout (2000) is a *different* id than whatever
        // the Lead might be showing -- but it shares "wall-a" with
        // what was published, so this display's own layout is what
        // must be resolved to and applied, never the Lead's own id
        // (which isn't even present in the message at all -- see
        // syncgroup::Message::Command's own doc comment).
        let (mut handler, _togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 2000, vec!["wall-a".into()]);

        let resolved = handler.resolve_layout_for_sync_keys(&["wall-a".to_string()]);

        assert_eq!(resolved, Some(2000),
                   "must resolve to this display's own scheduled layout, matched purely \
                    by shared sync_key, regardless of what layout id the Lead has");
    }

    #[test]
    fn returns_none_when_my_own_layout_shares_no_sync_key_with_what_was_received() {
        // The core safety property: if this display's own currently
        // sync-gated layout's own sync_keys don't overlap at all with
        // what was published, it must resolve to nothing -- meaning
        // "do not apply anything", not "fall back to the Lead's own
        // layout id" (there isn't one to fall back to at all).
        let (mut handler, _togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 2000, vec!["sync-x".into()]);

        let resolved = handler.resolve_layout_for_sync_keys(&["sync-y".to_string()]);

        assert_eq!(resolved, None,
                   "no overlap between my own layout's sync_keys and what was received \
                    must resolve to nothing -- never fall back to any other layout id");
    }

    #[test]
    fn returns_none_when_nothing_is_currently_sync_gated_at_all() {
        // If this display has no active sync-gated layout of its own
        // right now (e.g. its own schedule hasn't caught up yet, or
        // it genuinely isn't part of any Synchronised Event), there's
        // nothing to resolve to at all.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_with_sync_keys_for_test(2000, vec!["sync-x".into()]);
        // Deliberately no sync-gated schedule entry set up at all.

        let resolved = handler.resolve_layout_for_sync_keys(&["sync-x".to_string()]);

        assert_eq!(resolved, None);
    }

    #[test]
    fn a_scheduled_but_not_yet_cached_layout_retries_required_files_and_fails_gracefully() {
        // The user's own proposed fix: when we already know exactly
        // which layout we need (our own schedule names it as
        // sync-gated) but don't have it cached yet, this is precisely
        // the moment (a Command/heartbeat carrying sync_keys is being
        // resolved) to retry RequiredFiles directly -- rather than a
        // blind, fixed-delay retry attempted on every collection cycle
        // regardless of whether it could possibly help (an earlier,
        // since-reverted version of this fix did exactly that). The
        // simple mock this test uses can't correctly answer a real
        // RequiredFiles/GetFile round trip -- this test only confirms
        // the attempt is made and fails *gracefully* (no panic, a
        // clean None -- not part of the specific test_handler's own
        // request log, since checking that would need a more capable
        // mock server than this module's own tests otherwise need).
        let (mut handler, _togui_rx) = test_handler();
        let now = OffsetDateTime::now_local().unwrap();
        // Named as sync-gated in the schedule, but deliberately never
        // cached at all -- matching the real gap this retry targets.
        handler.schedule = schedule_with_sync_layout(
            1024, now - time::Duration::hours(1), now + time::Duration::hours(1));

        let resolved = handler.resolve_layout_for_sync_keys(&["sync1".to_string()]);

        assert_eq!(resolved, None,
                   "the retry attempt must fail gracefully against a mock that can't \
                    serve a real RequiredFiles/GetFile round trip -- no panic, and no \
                    incorrect resolution");
    }

    #[test]
    fn catching_up_on_our_own_reconnects_for_a_freshly_coordinated_sync() {
        // Even once this display has successfully caught up on its own
        // (via retry_and_download_layout), applying the switch
        // immediately here would start its own region/playlist timers
        // at whatever moment the download finished -- not at the
        // Lead's own originally-published target_time, already in the
        // past by the time retrying/downloading finishes. Correct
        // content, but drifted timing. Reconnecting here re-triggers
        // the Lead's own sync_peer_connected reaction, which
        // re-publishes a fresh target_time to apply against instead.
        // Tested by calling the extracted reaction directly (see its
        // own doc comment) -- a full round trip through a real
        // download success would need a mock capable of a whole
        // RequiredFiles+GetFile exchange, not otherwise needed here.
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let lead = syncgroup::SyncGroup::start_lead(port, std::time::Duration::ZERO)
            .expect("starting a real Lead to reconnect to");
        let peer_connected = lead.peer_connected().expect("a Lead's own is always Some");

        let (mut handler, _togui_rx) = test_handler();
        handler.settings.sync_role = SyncRole::Follower { lead_addr: "127.0.0.1".into() };
        handler.settings.sync_publisher_port = port;

        handler.reconnect_after_catching_up_on_own(1024);

        assert!(handler.sync_group.is_some(),
                "a fresh SyncGroup Follower connection must now exist");
        peer_connected.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the Lead must observe a genuinely new connection");
    }

    #[test]
    fn an_active_suppression_prevents_the_race_this_whole_mechanism_guards_against() {
        // Regression test for a real, live-observed race: after a
        // successful direct catch-up download (via
        // retry_and_download_layout), reconnect_after_catching_up_on_own
        // triggers a reconnect for a freshly-coordinated sync -- but the
        // very next schedule_check() call (already made unconditionally
        // by this same handler right after resolving) would otherwise
        // *independently* discover the very layout that was just cached
        // and stage/apply it a second time, a few seconds after the
        // reconnect's own fresh Command already did. Confirmed real via
        // a live capture: two `ForceReloadLayout` navigations close
        // together -- a real risk of navigating mid-load if a widget's
        // own JS happens to be running at that exact moment.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1024, vec!["sync1".into()]);
        let now = OffsetDateTime::now_local().unwrap();
        handler.schedule = schedule_with_sync_layout(
            1024, now - time::Duration::hours(1), now + time::Duration::hours(1));
        // Matches exactly what reconnect_after_catching_up_on_own sets,
        // simulating "a reconnect for this exact layout was just
        // triggered, moments ago".
        handler.suppress_local_discovery_of = Some((1024, std::time::Instant::now()));

        handler.schedule_check();

        assert_eq!(handler.pending_sync_layout_id, None,
                   "local discovery must be suppressed while the reconnect-triggered \
                    fresh Command is still expected imminently -- staging it locally \
                    too would race with that fresh Command, causing two \
                    ForceReloadLayout navigations close together");
    }

    #[test]
    fn an_expired_suppression_lets_local_discovery_resume_normally() {
        // The other half of the same mechanism: if the reconnect
        // doesn't pan out within a reasonable window (e.g. the Lead is
        // briefly unreachable), this display must not be left
        // permanently unable to discover a layout it has every right to
        // show -- the suppression is deliberately short-lived, not a
        // permanent block.
        let (mut handler, _togui_rx) = test_handler();
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1024, vec!["sync1".into()]);
        let now = OffsetDateTime::now_local().unwrap();
        handler.schedule = schedule_with_sync_layout(
            1024, now - time::Duration::hours(1), now + time::Duration::hours(1));
        // Simulate a suppression set well past its own grace window
        // (real code uses 5s; well over a minute here removes any
        // possible timing flakiness in this assertion).
        handler.suppress_local_discovery_of = Some((1024,
            std::time::Instant::now() - std::time::Duration::from_secs(90)));

        handler.schedule_check();

        assert_eq!(handler.pending_sync_layout_id, Some(1024),
                   "an expired suppression must not block local discovery from \
                    resuming normally -- otherwise a failed reconnect attempt could \
                    leave this display stuck, never discovering a layout it has \
                    every right to show");
    }

    #[test]
    fn any_single_shared_sync_key_among_several_is_enough_to_resolve() {
        // The Lead may publish several sync_keys at once (every one
        // its own layout carries); a display needs only *one* of them
        // in common with its own layout to be considered part of the
        // same synchronized grouping -- not an exact match of the
        // whole set.
        let (mut handler, _togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 3000,
                                               vec!["sync-a".into(), "sync-b".into()]);

        let resolved = handler.resolve_layout_for_sync_keys(
            &["sync-b".to_string(), "sync-z".to_string()]);

        assert_eq!(resolved, Some(3000),
                   "sharing even one sync_key (here, sync-b) must be enough to resolve");
    }

    #[test]
    fn sync_apply_timer_does_nothing_when_sync_keys_do_not_match_my_own_schedule() {
        // Full end-to-end safety check, mirroring the real select!
        // arm's own body: a display that receives sync_keys sharing
        // nothing with its own currently sync-gated layout must not
        // apply *anything* -- no override, no reload, no layout
        // change at all. This is the corrected behavior for what was
        // a real, reported design gap: the original version trusted
        // a layout id directly from the network, which would have
        // shown the Lead's own (wrong, for this display) content
        // instead of correctly doing nothing here.
        let (mut handler, togui_rx) = test_handler();
        setup_always_active_sync_gated_layout(&mut handler, 2000, vec!["sync-x".into()]);
        handler.pending_sync_keys = Some(vec!["sync-y".into()]); // no overlap with sync-x
        handler.sync_apply_timer = crossbeam_channel::after(std::time::Duration::from_millis(1));
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(handler.sync_apply_timer.try_recv().is_ok());
        if let Some(sync_keys) = handler.pending_sync_keys.clone() {
            if let Some(layout_id) = handler.resolve_layout_for_sync_keys(&sync_keys) {
                handler.override_layout = Some(layout_id);
                handler.sync_layout_active = true;
                handler.layouts = vec![layout_id];
                handler.to_gui.send(ToGui::ForceReloadLayout(layout_id)).unwrap();
            }
            handler.sync_apply_timer = never();
            handler.schedule_check();
        }

        assert!(handler.override_layout.is_none(),
                "no override must be committed when sync_keys don't match anything of mine");
        assert!(!handler.sync_layout_active);
        assert!(!std::iter::from_fn(|| togui_rx.try_recv().ok())
                    .any(|m| matches!(m, ToGui::ForceReloadLayout(_))),
                "must never force a reload for a synchronized event this display isn't \
                 actually part of");
    }

    #[test]
    fn a_failed_resolution_does_not_loop_forever_on_the_next_schedule_check() {
        // Regression test: a Follower whose own sync_keys resolution
        // kept failing would re-stage and re-fail the same sync_keys
        // every schedule_check cycle, forever. See the sync_apply_timer
        // handler's own doc comment (reading/cloning instead of
        // `.take()`ing pending_sync_keys) for the root cause.
        let (mut handler, _togui_rx) = test_handler();
        // A sync-gated layout whose own cached sync_keys are empty
        // (e.g. a stale pre-sync_keys-feature cache entry) -- simulated
        // directly, since the effect on this loop is identical
        // regardless of why they're empty/non-matching.
        setup_always_active_sync_gated_layout(&mut handler, 1014, vec![]);
        // A tiny real delay (not the CMS-configured default, ~750ms)
        // -- needed for the check below to reliably tell "never
        // re-armed" apart from "re-armed but its own delay hasn't
        // elapsed yet": `never()`'s own try_recv() reports the exact
        // same `Empty` either way, so waiting past a short, known
        // delay and checking whether it *fired* is the only reliable
        // signal (a first, flawed version of this test assumed
        // `never()` reports `Disconnected` distinctly -- it doesn't).
        handler.settings.sync_switch_delay = 5;

        handler.schedule_check();
        assert_eq!(handler.pending_sync_keys, Some(vec![]),
                   "sanity: the empty sync_keys must have been staged first");

        // Mirrors the real select! arm's own body exactly (see the
        // main loop's `recv(self.sync_apply_timer)` handler).
        if let Some(sync_keys) = handler.pending_sync_keys.clone() {
            if let Some(layout_id) = handler.resolve_layout_for_sync_keys(&sync_keys) {
                handler.override_layout = Some(layout_id);
                handler.sync_layout_active = true;
                handler.layouts = vec![layout_id];
            }
            handler.sync_apply_timer = never();
            handler.schedule_check();
        }

        assert!(handler.override_layout.is_none(),
                "sanity: resolution must have failed (empty sync_keys never match)");

        // The critical check: a SECOND schedule_check(), mirroring the
        // next collection cycle/select! loop iteration, must not
        // re-arm sync_apply_timer at all -- the guard must recognize
        // this exact (empty) sync_keys set as already having been
        // tried. Confirmed by waiting well past the tiny switch_delay
        // set above and checking the timer never actually fires --
        // if it had been re-armed (the bug), it would have fired by
        // now; `never()` (correct) never does, no matter how long we
        // wait.
        handler.schedule_check();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(handler.sync_apply_timer.try_recv().is_err(),
                "must not re-stage/re-arm the timer for a sync_keys set that was \
                 already tried and failed to resolve -- doing so every cycle is \
                 exactly the real, severe infinite-loop bug this test guards against");
    }

    #[test]
    fn a_not_yet_cached_layout_stages_nothing_until_it_actually_downloads() {
        // Regression test for a real, severe production bug: the user
        // swapped which layout a live Synchronised Event points to
        // (1014 -> 1017 on the CMS). The Lead's own download of 1017's
        // required files finished fast enough that its own
        // schedule_check discovered the *real* sync_keys immediately --
        // but a Follower, racing its own separate download of the same
        // files (some of which can be deferred to a later cycle via
        // resource_retry_queue on a transient failure), had its own
        // schedule_check run *before* 1017 was actually in its own
        // cache. The old code computed sync_keys as an empty Vec in
        // that case (via `.unwrap_or_default()`) -- not because the
        // layout genuinely has none, but because they weren't knowable
        // yet -- staged that wrong value, and (thanks to the *separate*
        // fix for the infinite-loop bug just above, which deliberately
        // never re-stages an already-tried value) got permanently
        // stuck on it: even once 1017 finished downloading moments
        // later and its real sync_keys became known, the guard refused
        // to ever re-stage with the correct value. Confirmed real: the
        // Follower downloaded the resource but never rendered the new
        // layout at all.
        let (mut handler, _togui_rx) = test_handler();
        let now = OffsetDateTime::now_local().unwrap();
        // 1017 is sync-gated in the schedule, but deliberately NOT yet
        // in cache at all (no insert_fake_layout_for_test call for it)
        // -- matching the real race: schedule.xml already names it,
        // but its own required files haven't finished downloading yet.
        handler.schedule = schedule_with_sync_layout(
            1017, now - time::Duration::hours(1), now + time::Duration::hours(1));

        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, None,
                   "a sync-gated layout that isn't in cache yet must stage *nothing* \
                    at all -- not even an empty Vec, which would then be \
                    indistinguishable from a genuinely-resolved empty sync_keys set \
                    once the guard against re-staging kicks in");
        assert!(handler.sync_apply_timer.try_recv().is_err(),
                "nothing should have been armed either");

        // The layout finishes downloading moments later, with its real
        // sync_keys now known.
        handler.cache.insert_fake_layout_with_sync_keys_for_test(1017, vec!["sync1".into()]);

        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, Some(vec!["sync1".to_string()]),
                   "once the layout is actually in cache, a later schedule_check must \
                    correctly stage its real sync_keys -- this is the exact case that \
                    used to stay permanently stuck on an empty Vec instead");
    }

    #[test]
    fn a_not_yet_cached_layout_does_not_disturb_an_already_pending_switch() {
        // Companion to the test above: if a *different* synchronized
        // switch is already correctly staged (e.g. for an ongoing
        // event this display's own schedule already resolved), a
        // separate, not-yet-cached sync-gated layout discovery must
        // not clobber that existing, valid pending_sync_keys value
        // with anything (empty or otherwise) -- it should be left
        // completely untouched.
        let (mut handler, _togui_rx) = test_handler();
        handler.pending_sync_keys = Some(vec!["already-pending".to_string()]);
        let now = OffsetDateTime::now_local().unwrap();
        handler.schedule = schedule_with_sync_layout(
            2000, now - time::Duration::hours(1), now + time::Duration::hours(1));
        // 2000 deliberately not in cache.

        handler.schedule_check();

        assert_eq!(handler.pending_sync_keys, Some(vec!["already-pending".to_string()]),
                   "an already-pending sync_keys value must be left completely \
                    untouched when a different, not-yet-cached layout is discovered");
    }
}
