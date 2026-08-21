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
use crate::config::{CmsSettings, PlayerSettings};
use crate::{logger, server, util, xmds, xmr};
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
}

pub enum Kill {
    No,
    Terminate,
    Kill,
}

/// Messages received from the GUI thread
pub enum FromGui {
    Showing(i64),
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
    current_layout: i64,
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
    /// Set once GetWeather comes back with a "Procedure ... not
    /// present" SOAP fault (see `is_method_not_present_fault`'s own
    /// doc comment) -- found from a real report: our own XMDS
    /// endpoint URL is hardcoded to `?v=5` (see xmds::Cms::new), but
    /// GetWeather/GetDependency/GetData are v6/v7-only additions, so a
    /// CMS whose v5 SOAP endpoint genuinely has no such method call
    /// will fail this way on *every single* collection cycle forever
    /// -- not a transient error worth retrying, logging a fresh
    /// warning about indefinitely. Once set, collect_once() skips the
    /// call entirely instead of repeating the same failed attempt.
    weather_unsupported: bool,
    /// The display_time_zone value actually applied to this process's
    /// own TZ environment variable so far this run (see
    /// apply_process_timezone's own doc comment) -- None until the
    /// first successful registration provides one. Tracked so a LATER
    /// change to the CMS's own setting is detected and reported (a
    /// restart is required to pick it up) rather than either silently
    /// ignored, or unsafely re-applied from update_settings()'s own
    /// later, multi-threaded calls (collect_once() runs on a separate
    /// thread from the one that first constructed this Handler).
    process_timezone_applied: Option<String>,
    /// Tracks whether this run has already triggered a screenshot for
    /// the *current* true value of settings.screen_shot_requested (see
    /// that field's own doc comment in config.rs) -- edge-triggered
    /// (fires once when the flag transitions false -> true), not
    /// level-triggered, so a flag that happens to stay true across
    /// several collection cycles (e.g. some delay before the CMS
    /// resets it after receiving our submission) doesn't cause a new
    /// screenshot every single cycle. Reset back to false once the
    /// flag itself goes back to false, ready to fire again next time.
    screenshot_requested_seen: bool,
    /// Timer for hiding/advancing the currently-shown overlay. Moved
    /// here (was local to `run()`) since `schedule_check()` needs to
    /// (re)schedule it too, not just XMR's `overlayLayout` handling.
    overlay_expiry: Receiver<std::time::Instant>,
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
}

/// See `Handler::resource_retry_queue`'s own doc comment.
///
/// Widened from the original 15s x 5 (75s total) -- proved too short
/// in a real case where the CMS's "Cache not ready" fault persisted
/// well over a minute. ~3 minute total window gives the CMS more
/// realistic room while still eventually giving up.
const RESOURCE_RETRY_DELAY: Duration = Duration::from_secs(20);
const RESOURCE_RETRY_MAX_ATTEMPTS: u32 = 8;
/// See `Handler::pending_auth`'s own doc comment. 30s strikes a balance
/// between responsiveness (someone actively watching for the display
/// to come online after approving it) and not hammering the CMS with
/// requests while genuinely just waiting.
const PENDING_AUTH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

impl Handler {
    /// Create a new handler, with channels to the GUI thread.
    // Not bundled into an options struct: purely a clippy style lint,
    // not worth touching all 9 call sites for.
    #[allow(clippy::too_many_arguments)]
    pub fn new(cms: &CmsSettings, clear_cache: bool, envdir: &Path,
               no_verify: bool, allow_offline: bool, debug_override: bool,
               to_gui: Sender<ToGui>, from_gui: Receiver<FromGui>,
               duration_rx: Receiver<server::DurationRequest>) -> Result<Self> {
        let (privkey, pubkey) = load_or_create_keypair(envdir)?;
        let cache = Cache::new(cms, envdir.join("res"), clear_cache, no_verify)
            .context("creating cache")?;
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
                                 layouts, envdir: envdir.into(), current_layout: 0,
                                 override_layout: None, overlay_layout: None,
                                 stats: StatCollector::default(),
                                 faults: faults::FaultCollector::default(),
                                 layout_playing_since: None,
                                 criteria: CriteriaStore::default(),
                                 shell_process: None, last_command_success: None,
                                 duration_rx, overlay_expiry: never(),
                                 schedule_overlays: Vec::new(), schedule_overlay_idx: 0,
                                 resource_retry_queue: Vec::new(),
                                 dataupdate_retry_queue: Vec::new(),
                                 resource_retry_timer: never(),
                                 pending_auth: false,
                                 pending_network: false,
                                 weather_unsupported: false,
                                 process_timezone_applied: None,
                                 screenshot_requested_seen: false,
                                 debug_override, xmr_privkey: privkey.clone(),
                                 xmr_retry_key, cms: cms.clone(), no_verify };
            slf.update_settings();
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
                                 layouts: vec![], envdir: envdir.into(), current_layout: 0,
                                 override_layout: None, overlay_layout: None,
                                 stats: StatCollector::default(),
                                 faults: faults::FaultCollector::default(),
                                 layout_playing_since: None,
                                 criteria: CriteriaStore::default(),
                                 shell_process: None, last_command_success: None,
                                 duration_rx, overlay_expiry: never(),
                                 schedule_overlays: Vec::new(), schedule_overlay_idx: 0,
                                 resource_retry_queue: Vec::new(),
                                 dataupdate_retry_queue: Vec::new(),
                                 resource_retry_timer: never(),
                                 pending_auth: !network_pending,
                                 pending_network: network_pending,
                                 weather_unsupported: false,
                                 process_timezone_applied: None,
                                 screenshot_requested_seen: false,
                                 debug_override, xmr_privkey: privkey.clone(),
                                 xmr_retry_key: Some(privkey),
                                 cms: cms.clone(), no_verify };
            slf.update_settings();
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
        loop {
            select! {
                // timer channel that fires when collect is needed
                recv(collect) -> _ => {
                    if let Err(e) = self.collect_once() {
                        log::error!("during collect: {e:#}");
                    }
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
                    // Diagnostic log (found genuinely useful investigating
                    // a real report: screenshots not reaching the CMS with
                    // --debug active, no error anywhere) -- confirms
                    // whether this timer arm actually fires and the
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
                // timer channel that fires to retry resources whose
                // download failed during a normal collection -- see
                // `resource_retry_queue`'s own doc comment.
                recv(self.resource_retry_timer) -> _ => {
                    self.retry_failed_resources();
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
                            log::error!("durign cache purge: {e:#}");
                        }
                        collect = after(Duration::from_secs(0));  // force re-download
                    }
                    Ok(xmr::Message::WebHook(code)) => {
                        self.to_gui.send(ToGui::WebHook(code)).unwrap();
                    }
                    Ok(xmr::Message::Command(code)) => {
                        self.run_command(&code);
                    }
                    Ok(xmr::Message::DataUpdate(widget_id)) => {
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
        // http|url|contentType|jsonBody or rs232|params|message form
        // (confirmed real convention), not just a shell line --
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
            if settings != self.settings {
                self.settings = settings;
                self.update_settings();
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
            // BUG fix (found from a direct question asking to verify
            // this exact scenario): a previously-authorized, normally-
            // running display (pending_auth/pending_network both
            // false) that gets deauthorized used to just `bail!()`
            // here -- logged as an error once per collection cycle,
            // but self.pending_auth was never set (so retries kept
            // using the slow, normal collect_interval instead of the
            // fast 30s one) and self.schedule/self.layouts were never
            // cleared (so the GUI was never told to switch away --
            // the display would keep cycling through its stale cached
            // schedule indefinitely, never reverting to the splash
            // screen the way a freshly-unauthorized display does).
            // Now: transitions into the same pending_auth state a
            // fresh, never-yet-authorized display starts in, and
            // actively clears the schedule + calls schedule_check()
            // immediately so schedule_check's own empty-schedule path
            // (see its own doc comment) sends ToGui::Layouts(vec![]),
            // which gui.rs's own Schedule::update() correctly resolves
            // to the splash screen (layout 0) as a last resort -- not
            // waiting for the next periodic schedule_check tick (60s).
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
        // keeps the scheduleid the same -- confirmed via two real
        // schedule.xml samples, before and after a real publish).
        let current_scheduleid = self.schedule.scheduleid_for(self.current_layout);

        // download all missing files
        let mut result = Vec::new();
        let total = required.len();
        // DownloadStartWindow/DownloadEndWindow gates only bulk file
        // downloads below, not the lightweight XMDS calls above --
        // schedule/layout-switching still needs current info even
        // outside the window, using whatever's already cached.
        if !self.settings.is_within_download_window() {
            log::info!("outside the configured download window \
                        ({}-{}), skipping {total} pending file download(s) \
                        this cycle", self.settings.download_start_window,
                       self.settings.download_end_window);
        } else {
        for (i, file) in required.into_iter().enumerate() {
            if !self.cache.has(&file)
               && !is_exempt_as_currently_playing_layout(&file, current_scheduleid,
                                                          &schedule,
                                                          self.settings.expire_modified_layouts) {
                let filedesc = file.description();
                let inventory = file.inventory();
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
                // Dependencies are excluded from the MediaInventory
                // report below (not pushed to `result` at all) --
                // unlike media/layout files, they have no single
                // meaningful integer id to report (see ReqFile::
                // Dependency's own doc comment), and the reference
                // client doesn't appear to report on them via
                // MediaInventory either. Reporting a synthetic
                // placeholder id for every dependency downloaded in
                // the same cycle would produce multiple identical,
                // meaningless entries in that report.
                let is_dependency = matches!(file, crate::resource::ReqFile::Dependency { .. });
                match self.cache.download(file.clone(), &mut self.xmds)
                                .with_context(|| format!("downloading {filedesc}"))
                {
                    Ok(()) => if !is_dependency { result.push((inventory, true)); },
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
        }

        // let the CMS know we have the media
        self.xmds.submit_media_inventory(result)?;

        // now that we should have all media, apply the schedule
        self.schedule = schedule;
        let _ = self.schedule.to_file(self.envdir.join("sched.json"));

        // Weather-derived Schedule Criteria (see xmds::Cms::get_weather's
        // own doc comment) -- a periodic pull, complementing (not
        // replacing) the existing xmr::Message::CriteriaUpdate push
        // path, both feeding the same self.criteria store. Errors here
        // are logged, not propagated -- a failed weather fetch
        // shouldn't fail this otherwise-successful collection cycle.
        // Skipped proactively while the known, hardcoded XMDS endpoint
        // version doesn't have GetWeather at all (see
        // xmds::xmds_supports_v6_v7_methods's own doc comment) -- no
        // point even trying a call known in advance to always fail.
        // weather_unsupported stays as a runtime-learned fallback for
        // the (currently not expected) case where that assumption
        // turns out wrong for some CMS -- once either signal is true,
        // stop calling for the rest of this session.
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
                Err(e) => log::warn!("getting weather: {e:#}"),
            }
        }

        self.schedule_check();

        // send log messages
        self.xmds.submit_log(&logger::pop_entries())?;

        self.send_status_update();

        self.flush_stats();
        self.flush_faults();

        log::info!("collection successful");
        Ok(())
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
        // BUG fix (found from a real report, confirmed via a real
        // Xibo issue -- #983, "the player should provide its timezone
        // to RegisterDisplay... stored on the display record" -- and
        // a real Android player bug report describing the exact same
        // symptom): reporting the *raw system* timezone here
        // unconditionally means an admin's manual displayTimeZone
        // override in the CMS gets silently overwritten on the very
        // next NotifyStatus (which happens often -- every collection
        // cycle, and immediately on every layout change). Reporting
        // the *effective* timezone we're already using (the CMS's own
        // display_time_zone, once it's told us one) instead means our
        // own report never fights a value the CMS already gave us --
        // only a brand-new, never-configured display (empty
        // display_time_zone) falls back to the system's own timezone,
        // preserving the original auto-detection use case for that
        // case specifically.
        let system_tz = util::timezone();
        let reported_tz = timezone_to_report(&self.settings.display_time_zone, &system_tz);
        let status = xmds::Status {
            currentLayoutId: self.current_layout,
            availableSpace: avail,
            totalSpace: total,
            lastCommandSuccess: self.last_command_success.unwrap_or(true),
            deviceName: &self.settings.display_name,
            timeZone: &reported_tz,
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
    fn flush_faults(&mut self) {
        if self.faults.is_empty() {
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

    fn schedule_check(&mut self) {
        // Prune expired Schedule Criteria before every evaluation, so a
        // stale value doesn't keep a criteria-conditioned layout active
        // (or, for a `ne` condition, incorrectly inactive) past its ttl.
        self.criteria.prune_expired();
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
            // BUG fix (found from a real crash report): this codebase's
            // own Logger (logger.rs) formats record.args() TWICE --
            // once for console output, once to build the string stored
            // for later upload to the CMS (SubmitLog). Most Display
            // impls are idempotent, so this is normally harmless, but
            // itertools::Format is explicitly single-use -- formatting
            // it a second time panics with "Format: was already
            // formatted once". Converting to a String *before* handing
            // it to log::warn! (matching the already-safe pattern just
            // below, at the new_layouts.iter().format(", ").to_string()
            // call) means the log macro only ever sees a plain,
            // idempotent String, regardless of how many times any
            // logger implementation formats its own arguments.
            let listed = new_layouts.iter().format(", ").to_string();
            log::warn!("none of the newly-scheduled layout(s) ({listed}) are cached yet, \
                        keeping whatever's currently showing until they are");
            self.layouts.clone()
        } else {
            available
        };
        if new_layouts != self.layouts {
            let all_layouts = new_layouts.iter().format(", ").to_string();
            log::info!("new layouts in schedule: {}", all_layouts);
            self.to_gui.send(ToGui::Layouts(new_layouts.clone())).unwrap();
            self.layouts = new_layouts;
        }
        self.recheck_schedule_overlays();
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

    /// Apply new player settings.
    fn update_settings(&mut self) {
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
                // Deliberately exits the whole process (rather than
                // just logging and continuing with the stale timezone
                // indefinitely) -- these totems are remote, unattended
                // kiosks, so a warning that requires someone to notice
                // it and manually restart is of little practical use.
                // A timezone change is rare, so a brief service
                // interruption for a clean restart (systemd's own
                // Restart=always, see arexibo.service) is a reasonable
                // trade-off against silently running with the wrong
                // timezone -- affecting schedule/download-window
                // evaluations -- until someone happens to notice.
                // Exit code 1 (not 0) for compatibility with either
                // Restart=always or Restart=on-failure policies,
                // matching the same convention already used for the
                // mainloop thread panic/exit handling in main.rs.
                log::error!("the CMS's own display timezone changed from {was:?} to \
                             {now:?} since this process started -- exiting so systemd \
                             can restart cleanly and pick up the change (changing a \
                             process's own timezone mid-session, from a background \
                             thread, isn't safe to do here)");
                std::process::exit(1);
            }
        }

        // Fallback for a screenshot request lost while offline (see
        // screen_shot_requested's own doc comment in config.rs) --
        // edge-triggered (see screenshot_requested_seen's own doc
        // comment) to avoid repeating this every cycle while the CMS's
        // own flag stays true.
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
        self.attempt_https_upgrade();

        // CMS-driven migration to a different server (see
        // new_cms_address/new_cms_key's own doc comment in config.rs)
        // -- deliberately cautious given how risky getting this wrong
        // would be (a totem could become unreachable). Only attempted
        // here, at the very end, after every other settings-derived
        // action above -- if this succeeds it exits the process.
        self.attempt_cms_migration();

        // let the GUI know to reconfigure itself
        self.to_gui.send(ToGui::Settings(Box::new(self.settings.clone()))).unwrap();
    }

    /// Upgrades this display's own CMS address from http:// to https://
    /// when the CMS says forceHttps is on -- see that field's own doc
    /// comment in config.rs. Validates the https:// address actually
    /// works first (reusing validate_new_cms, same reasoning as CMS
    /// migration -- READY *or* WAITING both count as success), only
    /// persisting and restarting on a confirmed-working upgrade. A
    /// failed validation is logged and left alone (still working over
    /// http, not broken) -- retried again next cycle for free, since
    /// this naturally stops being attempted at all once the address is
    /// already https://.
    fn attempt_https_upgrade(&mut self) {
        if !self.settings.force_https {
            return;
        }
        let Some(https_address) = https_upgrade_address(&self.cms.address) else {
            return; // already https://, or some other/unrecognized scheme
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
                return;
            }
        };

        match validate_new_cms(&candidate, pub_key, self.no_verify, self.envdir.join("xml")) {
            Ok(()) => {
                log::error!("https:// address confirmed working -- switching to it and \
                             exiting so systemd restarts cleanly");
                if let Err(e) = candidate.to_file(self.envdir.join("cms.json")) {
                    log::error!("writing cms.json for forceHttps upgrade: {e:#} -- NOT \
                                 exiting, will retry next cycle");
                    return;
                }
                std::process::exit(1);
            }
            Err(e) => {
                log::warn!("the https:// equivalent of our own CMS address didn't work, \
                            staying on http:// for now (will retry next cycle): {e:#}");
            }
        }
    }

    /// Attempts a CMS migration if the CMS has told us to (see
    /// new_cms_address/new_cms_key's own doc comment in config.rs).
    /// Validates the new CMS *before* committing anything -- a genuine
    /// SOAP-level success (READY *or* WAITING; a freshly-migrated
    /// display is very likely not yet authorized on the new CMS, and
    /// that alone must not be treated as a validation failure) confirms
    /// the new server is reachable and accepts this display's own
    /// identity, before any files on disk change. Does nothing at all
    /// (not even a log entry) if no migration is currently requested.
    fn attempt_cms_migration(&mut self) {
        if !should_attempt_cms_migration(&self.settings.new_cms_address, &self.settings.new_cms_key) {
            return;
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
                return;
            }
        };

        match validate_new_cms(&candidate, pub_key, self.no_verify, self.envdir.join("xml")) {
            Ok(()) => {
                log::info!("new CMS accepted registration -- committing migration");
                match commit_cms_migration(&self.envdir, &candidate) {
                    Ok(()) => {
                        log::error!("CMS migration complete -- exiting so systemd restarts \
                                     cleanly against the new CMS");
                        std::process::exit(1);
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
    }
}

/// Validates that a candidate new CMS is reachable and accepts this
/// display's own registration -- extracted as its own free function
/// (rather than inline in attempt_cms_migration) specifically so it
/// can be unit-tested directly against a real mock server, without
/// needing to go through the surrounding method that ends in
/// std::process::exit() on success. Ok(Some(_)) (READY, already
/// authorized) and Ok(None) (WAITING, not yet authorized) are both
/// treated as a successful Ok(()) -- see attempt_cms_migration's own
/// comment on why "not yet authorized" must not count as a validation
/// failure (a freshly-migrated display is very likely not yet
/// authorized on the new CMS, and that alone says nothing about
/// whether the new CMS is reachable/correctly configured).
fn validate_new_cms(candidate: &CmsSettings, pub_key: String, no_verify: bool,
                     xml_dir: PathBuf) -> Result<()> {
    let mut validation_cms = xmds::Cms::new(candidate, pub_key, no_verify, xml_dir)
        .context("constructing validation client")?;
    validation_cms.register_display().context("registering with the new CMS")?;
    Ok(())
}

/// Computes the https:// equivalent of `address` if it's currently
/// http:// -- literal scheme swap only, keeping host/port/path exactly
/// as configured (a deliberately simple interpretation of the real
/// mechanism confirmed via a real Xibo project issue, xibo-linux#247:
/// "swapped over to a HTTPS link" -- not attempting to guess a
/// different default port). Returns None if `address` doesn't start
/// with "http://" (already https://, or some other/unrecognized
/// scheme) -- nothing to upgrade.
fn https_upgrade_address(address: &str) -> Option<String> {
    address.strip_prefix("http://").map(|rest| format!("https://{rest}"))
}

/// Whether a CMS migration should be attempted -- both a new address
/// and a new key must be present (an empty value on either side means
/// no migration is currently requested by the CMS).
fn should_attempt_cms_migration(new_address: &str, new_key: &str) -> bool {
    !new_address.is_empty() && !new_key.is_empty()
}

/// The actual, disk-mutating part of a CMS migration -- separated from
/// attempt_cms_migration's own validation (network I/O against the new
/// CMS) and from the final process::exit() so this specific part can
/// be unit-tested directly (constructing a real temp directory,
/// checking the resulting files) without needing a real network call
/// or risking killing the test process. Only ever called *after* the
/// new CMS has already been validated as reachable and accepting.
fn commit_cms_migration(envdir: &Path, new_cms: &CmsSettings) -> Result<()> {
    let cms_path = envdir.join("cms.json");
    let backup_path = envdir.join("cms.json.bak");
    // A failed backup is logged but doesn't abort an otherwise-valid
    // migration -- losing the ability to manually inspect the old
    // settings later is a lesser problem than failing to migrate at
    // all after the new CMS has already confirmed it will accept us.
    if let Err(e) = fs::copy(&cms_path, &backup_path) {
        log::warn!("backing up cms.json before CMS migration: {e:#}");
    }
    new_cms.to_file(&cms_path).context("writing new cms.json")?;

    // Stale cache (layouts/media/resources) from the *old* CMS is at
    // best irrelevant and at worst actively misleading against a
    // different CMS (e.g. the same numeric layout id could mean
    // something completely different there) -- same clearing
    // mechanism already used for the existing --clear-cache flag (see
    // Cache::new's own `clear` parameter), just triggered here instead
    // of via a CLI flag on next start.
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

/// Whether `file` should be exempted from this cycle's re-download,
/// even though `Cache::has()` says it's stale (modified) -- because it
/// occupies the *same schedule slot* currently on screen, and
/// `expire_modified_layouts` says an in-progress modification/publish
/// should not interrupt it.
///
/// Uses `scheduleid`, NOT the raw layout id, to identify "the same
/// schedule slot" -- confirmed via two real schedule.xml samples taken
/// directly before and after a real publish of the layout being shown:
/// the layout id changed (925 -> 927, a new published version), but
/// `scheduleid="224"` stayed exactly the same. Comparing raw layout
/// ids (as an earlier version of this function did) can never detect
/// this common case at all, since publishing a layout is *specifically
/// what changes its id* -- the previous, id-based check only ever
/// caught the rarer case of a layout modified in place without a
/// separate publish step.
///
/// `current_scheduleid` must be looked up in the *previous* schedule
/// (before it gets replaced by the fresh one) -- the currently-playing
/// layout id may no longer even appear in the fresh schedule at all
/// (it's been superseded). `fresh_schedule` is then checked for
/// whether `file`'s own (new) layout id occupies that same scheduleid
/// now. A `current_scheduleid` of 0 (schedule.rs's own "no real
/// schedule entry" convention, e.g. currently showing the splash or
/// default layout) never exempts anything -- there's no real schedule
/// slot to match against.

/// Decides which timezone value to report to the CMS via NotifyStatus
/// -- see send_status_update's own comment at its call site for the
/// full context (a real report + a real Xibo issue confirming the CMS
/// stores whatever the player reports here back onto the display
/// record, silently overwriting an admin's manual override if we
/// always report the raw system timezone unconditionally). Prefers the
/// CMS's own display_time_zone (once it's told us one -- non-empty),
/// so our own report never fights a value the CMS already gave us;
/// falls back to the system's own timezone only for a brand-new,
/// never-configured display, preserving the original auto-detection
/// use case for that case specifically.
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
        // The exact fix for the real report: once the CMS has told us
        // a display_time_zone, our own report must echo that same
        // value back -- never the raw system one -- so it can never
        // silently overwrite an admin's manual CMS-side override.
        assert_eq!(timezone_to_report("Europe/London", "Europe/Rome"), "Europe/London");
    }

    #[test]
    fn falls_back_to_the_system_timezone_for_a_never_configured_display() {
        // A brand-new display the CMS has never told a timezone --
        // preserves the original auto-detection use case (Xibo issue
        // #983: "the player should provide its timezone... stored on
        // the display record").
        assert_eq!(timezone_to_report("", "Europe/Rome"), "Europe/Rome");
    }
}

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

/// Whether an error's own message indicates a "Procedure ... not
/// present" SOAP fault -- found from a real report: our own XMDS
/// endpoint URL is hardcoded to `?v=5` (see xmds::Cms::new), but
/// GetWeather/GetDependency/GetData are v6/v7-only additions, so a CMS
/// whose v5 SOAP endpoint genuinely has no such method call fails
/// this way -- permanently, not transiently, since retrying the exact
/// same call against the exact same endpoint version will never
/// succeed. This exact string is PHP SOAP's own generic "unknown
/// method" fault wording (not something Xibo-specific), so this same
/// check is reusable for any v6/v7-only method call, not just
/// GetWeather.
fn is_method_not_present_fault(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("not present")
}

// POSIX's own tzset(3) -- glibc does NOT re-parse the TZ environment
// variable just because it changed; without calling this explicitly
// after std::env::set_var("TZ", ...), time::OffsetDateTime::now_local()
// keeps returning the offset it had before the change. Verified
// experimentally (a standalone check, not just reasoning from docs):
// without this call, three different TZ values in a row all produced
// the same, unchanged +00:00:00 offset -- setting TZ alone silently
// had no effect at all. A single extern "C" declaration for one POSIX
// function, rather than pulling in the whole `libc` crate as a new
// dependency just for this.
extern "C" {
    fn tzset();
}

/// Sets *this process's own* TZ environment variable to the CMS's
/// display_time_zone (e.g. "Europe/Rome") -- so `arexibo`'s own
/// schedule/download-window evaluations (which read
/// time::OffsetDateTime::now_local(), itself respecting TZ) use the
/// CMS-specified timezone directly, without needing to touch this
/// machine's own system-wide /etc/localtime or /etc/timezone, and
/// without affecting any other process on the same machine. This is
/// the standard POSIX mechanism for "override one process's own idea
/// of local time" -- setting TZ does not change the system timezone.
///
/// Self-correcting rather than merely alerting: even a totem whose
/// system timezone ended up wrong (despite an autoinstall image
/// intending to force it -- manual setup, hardware clock issue, human
/// error) still evaluates schedules/download windows correctly, using
/// what the CMS's own Display record says this display should be in,
/// rather than requiring someone to notice a warning in the logs and
/// go fix the system's own configuration.
///
/// SAFETY-ADJACENT: only call this once, very early, before any other
/// thread has been spawned (main.rs's own Handler::new() call, before
/// its own `std::thread::spawn` and before gui::run()) -- mutating
/// process environment variables concurrently with another thread
/// reading them is a real, platform-level hazard regardless of
/// whether a given Rust toolchain's std::env::set_var happens to
/// require an `unsafe` block or not (this crate's own `time`
/// dependency gates its equivalent functionality behind an opt-in
/// "local-offset" feature for exactly this reason). See
/// process_timezone_applied's own doc comment for how the *caller*
/// (update_settings) avoids ever calling this again from a later,
/// multi-threaded context.
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
    // SAFETY: tzset() only reads the TZ environment variable and
    // updates process-global libc state describing the current
    // timezone rules -- it does not take/return pointers we own, and
    // is safe to call from a single thread (see apply_process_timezone's
    // own SAFETY-ADJACENT doc comment for why this whole function is
    // only ever called once, early, before any other thread exists).
    unsafe { tzset(); }
}

/// What update_settings should do about the CMS's own display_time_zone,
/// given what (if anything) has already been applied this run -- see
/// apply_process_timezone's own doc comment for the full context.
/// Extracted as its own pure decision (computed, not acted upon) so it
/// can be unit-tested directly without mutating this whole test
/// binary's own real TZ environment variable, which is process-global
/// and would risk flaky interference with any other test that also
/// happens to read it.
#[derive(Debug, PartialEq, Eq)]
enum TimezoneAction {
    /// Nothing to do -- either the CMS didn't send one, or it matches
    /// what's already applied.
    DoNothing,
    /// Not yet applied this run -- safe to apply now (only ever
    /// reached from the single-threaded window at startup, see
    /// apply_process_timezone's own SAFETY-ADJACENT note).
    Apply(String),
    /// Already applied a *different* value this run -- changing it
    /// again from here would be from a later, potentially
    /// multi-threaded context, so report instead of acting.
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
                        // Deliberately minimal -- every field beyond
                        // `code="READY"` has a graceful fallback default
                        // (see section 59's own fixes), so this alone
                        // must be enough to parse successfully.
                        r#"<ActivationMessage code="READY"/>"#.to_string()
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

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx)
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

        // Call 1 (inside Handler::new itself).
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx)
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
            let mut n = 0usize;
            for request in server.incoming_requests() {
                let code = codes.get(n).copied().unwrap_or(*codes.last().unwrap());
                n += 1;
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

        // Call 1 (Handler::new, "READY"): normal, authorized startup.
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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
        // Regression test for a real report: a totem's very first boot
        // (no cached settings.json yet), with WiFi not yet having
        // obtained an IP/working DNS -- register_display() genuinely
        // fails (not just a "not yet authorized" answer, an actual
        // connection failure) with --allow-offline set. This used to
        // bail!() the whole process out, causing systemd's Restart= to
        // redo the entire Xorg/D-Bus/arexibo startup sequence
        // repeatedly until the network happened to come up in the
        // brief window before the next attempt -- appearing "stuck".
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

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx)
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

        // Call 1 (inside Handler::new): gets the real address.
        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, "ws://127.0.0.1:8080");

        let _ = handler.collect_once();
        assert_eq!(handler.settings.xmr_web_socket_address_in_use, format!("ws://127.0.0.1:{port}/xmr"),
                   "our own derived /xmr default must win, even though it's port-shaped \
                    differently than the previously-cached address -- it's our own \
                    intentional fallback, not a suspicious CMS inconsistency");
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
        // Regression test for a real report: the CMS's own "Current
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();

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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
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

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();

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

        let handler = Handler::new(&cms, false, &envdir, true, true, false,
                                    togui_tx, fromgui_rx, duration_rx).unwrap();

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
        // The exact real report: our own v5-hardcoded XMDS endpoint
        // (see xmds::Cms::new) genuinely has no GetWeather method
        // (v6/v7-only) -- confirmed via a real warning log from an
        // actual totem.
        let err = anyhow::anyhow!("getting weather: parsing GetWeather SOAP response: \
                                    got SOAP fault: Procedure 'GetWeather' not present");
        assert!(is_method_not_present_fault(&err));
    }

    #[test]
    fn does_not_misclassify_an_unrelated_error() {
        // A genuine network/auth failure must NOT be treated as
        // "permanently unsupported" -- it should keep retrying
        // normally, unlike a real method-not-present fault.
        let err = anyhow::anyhow!("getting weather: io: connection refused");
        assert!(!is_method_not_present_fault(&err));
    }
}

#[cfg(test)]
mod decide_timezone_action_tests {
    use super::*;

    #[test]
    fn applies_on_first_registration() {
        // The safe, single-threaded window at startup (see
        // apply_process_timezone's own SAFETY-ADJACENT note) -- nothing
        // applied yet this run, the CMS sent a real value.
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
        // Must NOT call apply_process_timezone again here -- this path
        // is reached from update_settings() being called repeatedly
        // from collect_once(), which runs on a separate thread from
        // the one that first constructed this Handler (see
        // process_timezone_applied's own doc comment).
        assert_eq!(
            decide_timezone_action(&Some("Europe/Rome".to_string()), "America/New_York"),
            TimezoneAction::WarnRestartNeeded {
                was: "Europe/Rome".to_string(), now: "America/New_York".to_string(),
            });
    }

    #[test]
    fn does_nothing_when_the_cms_sends_nothing() {
        // An empty value is "the CMS didn't send this field", not "the
        // CMS wants an empty timezone" -- nothing to apply.
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
        // End-to-end check, not just the decision logic above --
        // std::env::set_var("TZ", ...) alone was verified experimentally
        // to have NO effect on time::OffsetDateTime::now_local() without
        // also calling tzset() afterward (three different TZ values in
        // a row all produced the same unchanged offset without it) --
        // this test exists specifically to catch that exact class of
        // regression, which a pure decision-logic test can't.
        //
        // NOTE: TZ is process-global process state; no other test in
        // this codebase currently reads/sets it, so this is safe today,
        // but a new test relying on a *specific* current offset without
        // itself pinning TZ could flake if it happens to run
        // concurrently with this one.
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

    /// Builds a real Schedule from XML, same wide-open from/to window
    /// convention as the real schedule.xml samples this whole fix is
    /// based on (`fromdt="1970-01-01 01:00:00" todt="2038-01-19
    /// 04:14:07"` -- always "currently active" regardless of when the
    /// test itself runs).
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
        // The exact real scenario confirmed via two real schedule.xml
        // samples taken directly before/after a real publish: layout
        // 925 got republished as 927 (a NEW layout id -- publishing is
        // specifically what changes it), but scheduleid="224" stayed
        // exactly the same in both. current_scheduleid is looked up in
        // the *old* schedule (925 -> 224); the fresh schedule now maps
        // 927 -> 224 too -- same slot, must be exempted.
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(927, 224);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert_eq!(current_scheduleid, 224, "sanity check on the old schedule's own lookup");
        assert!(is_exempt_as_currently_playing_layout(&layout_file(927), current_scheduleid,
                                                        &fresh_schedule, false),
                "a republished layout occupying the same schedule slot must be exempted -- \
                 comparing raw layout ids (925 != 927) would incorrectly NOT exempt this, \
                 which is the exact bug this fix addresses");
    }

    #[test]
    fn does_not_exempt_a_genuinely_different_schedule_slot() {
        // A DIFFERENT layout (different scheduleid) being modified must
        // still get its normal validity check -- only the slot
        // currently on screen is exempted.
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(913, 225);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert!(!is_exempt_as_currently_playing_layout(&layout_file(913), current_scheduleid,
                                                         &fresh_schedule, false));
    }

    #[test]
    fn does_not_exempt_anything_when_expire_modified_layouts_is_true() {
        // The CMS explicitly wants even the currently playing schedule
        // slot refreshed immediately when modified/republished.
        let old_schedule = schedule_with(925, 224);
        let fresh_schedule = schedule_with(927, 224);
        let current_scheduleid = old_schedule.scheduleid_for(925);
        assert!(!is_exempt_as_currently_playing_layout(&layout_file(927), current_scheduleid,
                                                         &fresh_schedule, true));
    }

    #[test]
    fn does_not_exempt_non_layout_files() {
        // Only layout-type files participate in this mechanism -- a
        // media file happening to share the same numeric id as the
        // republished layout must not be exempted.
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
        // 0 is schedule.rs's own "no real schedule entry" convention
        // (e.g. currently showing the splash screen or the default
        // layout, neither of which is a real <layout> schedule entry)
        // -- there's no genuine schedule slot to match against, so
        // nothing should ever be exempted on that basis.
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

    /// Counts how many ToGui::Screenshot messages are currently queued
    /// (draining the channel), ignoring any other ToGui variant (e.g.
    /// the ToGui::Settings that update_settings() always also sends).
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
        // The real mechanism this fixes (confirmed via the actual CMS
        // PHP source, Controller/Display.php's own requestScreenShot()):
        // screenShotRequested is a fallback for a screenshot request
        // whose real-time XMR push was lost (e.g. display offline at
        // the time) -- reported honestly on every subsequent
        // RegisterDisplay call until fulfilled. Edge-triggered: must
        // fire once when the flag becomes true, and must NOT fire
        // again on a later cycle where the CMS's own flag simply
        // hasn't been reset back to false yet (avoiding a new
        // screenshot every single collection cycle in that window).
        let port = start_mock_ready();
        let cms = test_cms_settings(port);
        let envdir = test_envdir();
        let (togui_tx, togui_rx) = crossbeam_channel::bounded(10);
        let (_fromgui_tx, fromgui_rx) = crossbeam_channel::bounded(5);
        let (_duration_tx, duration_rx) = crossbeam_channel::bounded(5);

        let mut handler = Handler::new(&cms, false, &envdir, true, true, false,
                                        togui_tx, fromgui_rx, duration_rx).unwrap();
        // Drain whatever update_settings() sent during construction.
        let _ = count_screenshot_messages(&togui_rx);

        handler.settings.screen_shot_requested = true;
        handler.update_settings();
        assert_eq!(count_screenshot_messages(&togui_rx), 1,
                   "must fulfill the pending request exactly once when the flag becomes true");

        // Simulate the flag staying true for a second cycle (CMS
        // hasn't reset it yet) -- must NOT fire again.
        handler.update_settings();
        assert_eq!(count_screenshot_messages(&togui_rx), 0,
                   "must not repeat the screenshot every cycle while the flag simply stays true");

        // CMS resets the flag once it receives our submission.
        handler.settings.screen_shot_requested = false;
        handler.update_settings();
        assert_eq!(count_screenshot_messages(&togui_rx), 0);

        // A genuinely new request later must fire again.
        handler.settings.screen_shot_requested = true;
        handler.update_settings();
        assert_eq!(count_screenshot_messages(&togui_rx), 1,
                   "a new request (flag re-transitioning false -> true) must fire again");
    }
}

#[cfg(test)]
mod https_upgrade_address_tests {
    use super::*;

    #[test]
    fn swaps_http_to_https_keeping_everything_else() {
        // Deliberately literal scheme swap only -- confirmed real
        // mechanism (xibo-linux#247): "swapped over to a HTTPS link",
        // not a different default port.
        assert_eq!(https_upgrade_address("http://cms.example.com"),
                   Some("https://cms.example.com".to_string()));
        assert_eq!(https_upgrade_address("http://192.168.2.138:9092"),
                   Some("https://192.168.2.138:9092".to_string()),
                   "an explicit port must be kept exactly as configured");
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
        assert_eq!(written.display_id, "stable-hw-id-123",
                   "the stable hardware identity must be preserved, not the old CMS's own value");

        let backed_up = CmsSettings::from_file(envdir.join("cms.json.bak")).unwrap();
        assert_eq!(backed_up.address, "https://old.example.com",
                   "the old settings must be backed up before being overwritten");
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

        assert!(!cache_dir.join("stale_layout.xlf.html").exists(),
                "stale cache from the old CMS must be cleared -- it's at best irrelevant and \
                 at worst actively misleading against a different CMS");
    }

    #[test]
    fn succeeds_even_without_a_pre_existing_cache_directory() {
        // A totem that has never actually downloaded anything yet (or
        // one where "res" was already removed for some other reason)
        // must not fail the migration just because there's nothing to
        // clear.
        let envdir = test_envdir();
        old_cms().to_file(envdir.join("cms.json")).unwrap();
        let new_cms = CmsSettings { address: "https://new.example.com".into(), key: "newkey".into(),
                                     display_id: "stable-hw-id-123".into(), display_name: None,
                                     proxy: None };
        assert!(commit_cms_migration(&envdir, &new_cms).is_ok());
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
        // The exact scenario flagged directly: a freshly-migrated
        // display is very likely not yet authorized on the new CMS --
        // that alone must not block the migration, since it says
        // nothing about whether the new CMS is reachable/correctly
        // configured. Matches the same "not an error, just not ready
        // yet" reasoning pending_auth already uses elsewhere.
        let port = start_mock(r#"<ActivationMessage code="WAITING"/>"#);
        let candidate = CmsSettings { address: format!("http://127.0.0.1:{port}"), key: "k".into(),
                                       display_id: "d".into(), display_name: None, proxy: None };
        assert!(validate_new_cms(&candidate, "dummy-pub-key".into(), true, test_xml_dir()).is_ok(),
                "WAITING (not yet authorized) must count as a successful validation, not a failure");
    }

    #[test]
    fn an_unreachable_server_fails_validation() {
        // A genuinely unreachable address (nothing listening) --
        // simulates the new CMS being misconfigured/down, which must
        // block the migration.
        let unreachable_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let candidate = CmsSettings { address: format!("http://127.0.0.1:{unreachable_port}"),
                                       key: "k".into(), display_id: "d".into(), display_name: None,
                                       proxy: None };
        assert!(validate_new_cms(&candidate, "dummy-pub-key".into(), true, test_xml_dir()).is_err(),
                "an unreachable new CMS must fail validation, not be silently treated as OK");
    }
}
