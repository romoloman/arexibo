// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Sync Group (video wall) LAN command channel.
//!
//! Confirmed real from two live register.xml captures (a Lead and a
//! Follower in the same Sync Group, same CMS -- see config::SyncRole's
//! own doc comment): a Follower learns its own Lead's real LAN IP
//! address directly from the CMS (`<syncGroup>`), and the CMS's own
//! manual confirms displays "communicate using their LAN IP address
//! over TCP on the publisher port specified" (default 9590) -- no
//! discovery mechanism of our own needed, unlike an earlier, rejected
//! design that invented LAN multicast discovery to solve a problem
//! (the CMS not knowing a Lead's real LAN IP behind NAT) that the real
//! data shows doesn't actually apply here.
//!
//! Transport: a minimal hand-rolled ZMTP PUB/SUB implementation,
//! matching the same protocol/technology Xibo already uses for XMR
//! (confirmed: XMR's own "ZMQ Pub Socket", and Sync Group's own
//! "publisher port" terminology) -- the SUB side reuses xmr.rs's own
//! `ZmqSubSocket` directly (already written, tested, and in production
//! use for XMR's own non-WebSocket transport); the PUB (server) side
//! is new here, since nothing in this codebase previously needed to
//! *accept* ZMTP connections rather than just make them.
//!
//! Message format: our own explicit JSON, not an attempt to match the
//! undocumented C# reference client's own wire format (interoperating
//! with a non-arexibo Sync Group member is explicitly not a
//! requirement here) -- two message kinds on the same channel:
//!
//! - `Sync`: the Lead's own current clock reading, sent periodically
//!   (independent of any layout change) so a Follower can track the
//!   clock offset between the two machines without depending on both
//!   having perfectly synchronized system clocks (e.g. via NTP) --
//!   the user's own proposed design.
//! - `Command`: "show this layout at this (Lead-clock) instant" -- a
//!   Follower applies its own most recently tracked offset to convert
//!   this to a local instant before acting on it, so all displays in
//!   the group switch at approximately the same real-world moment
//!   regardless of any raw clock skew between them.
//!
//! Video-wall-specific commands (e.g. frame-accurate synchronized
//! video start/pause) are deliberately not part of this first cut --
//! left as a natural, focused extension of this same message enum and
//! channel once needed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};
use anyhow::{bail, Context, Result};
use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};
use time::{Duration as TimeDuration, OffsetDateTime};
use crate::xmr::{retry_on_eintr, ZmqSubSocket};

/// How often the Lead sends a `Sync` message to every connected
/// Follower, independent of any layout change -- keeps each
/// Follower's own tracked clock offset fresh across a long-running
/// session, rather than computing it once at connect time and letting
/// it drift stale (real clocks, even NTP-disciplined ones, drift
/// slowly over hours/days).
const SYNC_INTERVAL: StdDuration = StdDuration::from_secs(5);

/// A Follower's own blocking-read timeout on its ZMTP connection to
/// the Lead -- see this module's own call to ZmqSubSocket::connect
/// for the full reasoning (short, relative to SYNC_INTERVAL, both to
/// detect a dead Lead promptly and to keep shutdown latency low).
const READER_TIMEOUT: StdDuration = StdDuration::from_secs(12);

/// How often a Follower's own reconnect-with-backoff loop (and the
/// Lead's own accept loop) re-checks its `stop` flag while otherwise
/// idle/blocked -- see SyncGroup's own Drop impl.
const STOP_CHECK_INTERVAL: StdDuration = StdDuration::from_millis(500);

const MAX_RECONNECT_BACKOFF: StdDuration = StdDuration::from_secs(30);

/// The Lead's "video ready" reports listener (Follower -> Lead) binds
/// on `port + REPORTS_PORT_OFFSET` -- a real ZMTP PUB socket can't
/// carry data in this direction at all, so this is a plain, separate
/// TCP connection (one newline-delimited JSON line per report, not
/// ZMTP-framed) rather than an extension of the existing channel.
const REPORTS_PORT_OFFSET: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Message {
    Sync {
        #[serde(with = "time::serde::rfc3339")]
        lead_time: OffsetDateTime,
        // Absent (deserializes to an empty Vec, via #[serde(default)])
        // when talking to a peer running an older build that never
        // sent this field at all -- treated the same as "Lead has
        // nothing currently sync-gated", never a parse error.
        #[serde(default)]
        current_sync_keys: Vec<String>,
    },
    Command {
        // Deliberately not a layout id at all -- a real safety gap
        // found this way: for anything other than Mirror Sync (Wall
        // Sync: each display shows a *different* layout of its own),
        // a Follower blindly applying the Lead's own layout id would
        // show the wrong content entirely. `syncKey` (confirmed real,
        // on a <region>/<drawer> in the real XLF -- see
        // layout::Translator's own `sync_keys` doc comment) is
        // per-region, not per-layout, matched independently by every
        // display against its *own* currently-scheduled layout (see
        // mainloop.rs's own resolve_layout_for_sync_keys) -- carrying
        // every sync_key this Lead's own currently-active layout has,
        // not just one, so a Follower whose own layout shares *any*
        // of them knows to reload.
        sync_keys: Vec<String>,
        #[serde(with = "time::serde::rfc3339")]
        target_time: OffsetDateTime,
    },
    // Video wall (Sync Group Phase 5): broadcast Lead -> every
    // Follower, reusing this same PUB/SUB channel (no offset
    // correction applied to these -- only Command's own target_time
    // needs that, since these three are reactive/ongoing, not a single
    // future instant to hit precisely).
    // `round`/`item` default to 0 so messages from a version without them
    // still decode (single-video behaviour).
    VideoPreload { sync_key: String, target: String,
                   #[serde(default)] item: u32, #[serde(default)] round: u32 },
    VideoPlay { sync_key: String, #[serde(default)] round: u32 },
    VideoPosition { sync_key: String, position_ms: i64, #[serde(default)] round: u32 },
}

/// A layout-switch command received by a Follower, with the tracked
/// clock offset already applied -- `target_local` is this *local*
/// clock's own equivalent instant to actually switch at. Deliberately
/// not carrying a `std::time::Instant` or owning a timer itself --
/// the caller (mainloop.rs) turns this into an actual timer the same
/// way every other scheduled revert/expiry there already works (e.g.
/// `after((target_local - now).unsigned_abs())`), keeping this module
/// unaware of mainloop.rs's own event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCommand {
    pub sync_keys: Vec<String>,
    pub target_local: OffsetDateTime,
}

/// A video-wall message received by a Follower, passed through
/// unchanged (no clock-offset correction -- see the Message enum's own
/// doc comment on why only Command needs that).
#[derive(Debug, Clone, PartialEq)]
pub enum VideoCommand {
    Preload { sync_key: String, target: String, item: u32, round: u32 },
    Play { sync_key: String, round: u32 },
    Position { sync_key: String, position_ms: i64, round: u32 },
}

/// Wire format only (see `send_video_ready_report`) -- carries no
/// follower identity of its own; the Lead learns *which* Follower sent
/// it from the TCP connection itself (see VideoReadyReport).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoReady {
    pub sync_key: String,
    #[serde(default)]
    pub round: u32,
}

/// A "buffered, ready to play" confirmation, enriched with the
/// reporting Follower's own address (from the reports connection's own
/// peer address, not anything the Follower claims about itself) --
/// needed so the Lead's own orchestration (mainloop.rs) can tell *how
/// many distinct* Followers have confirmed for the current round, not
/// just that *some* report arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoReadyReport {
    pub sync_key: String,
    pub round: u32,
    pub follower_addr: String,
}

/// Handle to a running Sync Group Lead or Follower background thread
/// (or thread pair, for a Lead: one accepting connections, one
/// publishing to them). Dropping this signals the thread(s) to stop
/// and release the port/connection -- see the CMS-assignable-role
/// change handling in mainloop.rs's own update_sync_group, which
/// replaces a SyncGroup outright (via simple reassignment, Rust
/// dropping the old value automatically) whenever the CMS's own
/// sync_role *kind* changes; a Follower's own lead_addr changing
/// while staying a Follower is handled the same way, deliberately,
/// rather than trying to rebind an existing connection in place --
/// simpler, and reconnecting from scratch is cheap.
pub struct SyncGroup {
    stop: Arc<AtomicBool>,
    /// Some for a Follower only -- see `commands()`.
    commands: Option<Receiver<SyncCommand>>,
    /// Some for a Lead only -- see `publish_sync_keys()`.
    publish: Option<Sender<Vec<String>>>,
    /// (Lead only, None for a Follower's own SyncGroup.) The
    /// currently-*committed* sync_keys -- i.e. what this Lead's own
    /// currently-showing layout actually has (empty = nothing
    /// sync-gated right now), not a switch merely pending during
    /// `switch_delay` -- broadcast on every periodic `Sync` heartbeat
    /// alongside `lead_time`, so a Follower that (re)connects mid-way
    /// through an already-ongoing Synchronised Event (a real gap found
    /// this way: a Follower restarted while the Lead was already
    /// showing a synced layout had no way to ever catch up, since the
    /// Lead only publishes a fresh `Command` when it *notices a
    /// change* -- nothing changes from the Lead's own perspective for
    /// an already-settled event) can immediately catch up instead of
    /// waiting indefinitely for a `Command` that will never come.
    /// Deliberately updated by the *caller* (mainloop.rs, via
    /// `set_current_sync_keys`) only once it has itself actually
    /// committed the switch (see mainloop.rs's own sync_apply_timer
    /// handler) -- never while merely staged/pending -- so a Follower
    /// checking this can never be told about a switch before the
    /// coordinated moment it was meant to happen at.
    current_sync_keys: Option<Arc<Mutex<Vec<String>>>>,
    /// (Lead only, None for a Follower's own SyncGroup.) Fires once
    /// for every Follower connection accept_loop successfully accepts
    /// -- see its own doc comment for why mainloop.rs needs to react
    /// to this by re-staging/re-publishing the currently-active
    /// synchronized layout, not just noting the connection.
    peer_connected: Option<Receiver<()>>,
    /// (Lead only.) Sends a video-wall broadcast request to
    /// `lead_publish_loop`, which turns it into an actual `Message` and
    /// broadcasts it -- see `publish_video_preload`/`_play`/`_position`.
    video_publish: Option<Sender<VideoPublishRequest>>,
    /// (Follower only.) Incoming video-wall messages, passed through
    /// unchanged from the wire (see VideoCommand's own doc comment).
    video_commands: Option<Receiver<VideoCommand>>,
    /// (Lead only.) "Ready to play" reports received from any Follower
    /// over the separate reports channel.
    video_ready: Option<Receiver<VideoReadyReport>>,
    /// (Follower only.) The Lead's reports-channel address
    /// (`host:port+REPORTS_PORT_OFFSET`) -- see `report_video_ready`.
    reports_addr: Option<String>,
    /// (Lead only.) The same list `accept_loop`/`lead_publish_loop`
    /// share -- exposed so `connected_follower_addrs` can query who's
    /// currently connected without needing its own IPC round-trip.
    peers: Option<Arc<Mutex<Vec<TcpStream>>>>,
}

/// Internal request from SyncGroup's own public API to `lead_publish_loop`
/// -- turned into the matching `Message` variant and broadcast from
/// there (not constructed by callers directly, so the wire format stays
/// this module's own implementation detail).
enum VideoPublishRequest {
    Preload { sync_key: String, target: String, item: u32, round: u32 },
    Play { sync_key: String, round: u32 },
    Position { sync_key: String, position_ms: i64, round: u32 },
}

impl Drop for SyncGroup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl SyncGroup {
    /// Starts a Lead: binds `port` on every local address (0.0.0.0,
    /// so Followers on the LAN can actually reach it -- this is a
    /// deliberate, narrow exception to normally binding internal
    /// servers to loopback only, since Sync Group's own entire
    /// purpose is being reachable from other machines on the LAN),
    /// accepts any number of Follower connections, and sends each one
    /// a `Sync` message every `SYNC_INTERVAL` alongside any `Command`
    /// requested via `publish_sync_keys`.
    pub fn start_lead(port: u16, switch_delay: StdDuration) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))
            .with_context(|| format!("binding Sync Group Lead listener on port {port}"))?;
        listener.set_nonblocking(true)
            .context("setting Sync Group Lead listener non-blocking")?;
        let reports_port = port + REPORTS_PORT_OFFSET;
        let reports_listener = TcpListener::bind(("0.0.0.0", reports_port))
            .with_context(|| format!("binding Sync Group reports listener on port {reports_port}"))?;
        reports_listener.set_nonblocking(true)
            .context("setting Sync Group reports listener non-blocking")?;

        let stop = Arc::new(AtomicBool::new(false));
        let peers: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let (publish_tx, publish_rx) = unbounded::<Vec<String>>();
        let (video_publish_tx, video_publish_rx) = unbounded::<VideoPublishRequest>();
        let (video_ready_tx, video_ready_rx) = unbounded::<VideoReadyReport>();
        let current_sync_keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (peer_connected_tx, peer_connected_rx) = unbounded::<()>();

        {
            let stop = stop.clone();
            let peers = peers.clone();
            thread::spawn(move || accept_loop(listener, peers, stop, peer_connected_tx));
        }
        {
            let stop = stop.clone();
            let current_sync_keys = current_sync_keys.clone();
            let peers = peers.clone();
            thread::spawn(move || lead_publish_loop(peers, publish_rx, video_publish_rx, stop,
                                                      switch_delay, current_sync_keys));
        }
        {
            let stop = stop.clone();
            thread::spawn(move || reports_accept_loop(reports_listener, video_ready_tx, stop));
        }

        log::info!("Sync Group: started as Lead, listening on 0.0.0.0:{port} \
                     (reports on 0.0.0.0:{reports_port})");
        Ok(Self { stop, commands: None, publish: Some(publish_tx),
                  current_sync_keys: Some(current_sync_keys),
                  peer_connected: Some(peer_connected_rx),
                  video_publish: Some(video_publish_tx), video_commands: None,
                  video_ready: Some(video_ready_rx), reports_addr: None,
                  peers: Some(peers) })
    }

    /// Starts a Follower: connects (and, on failure or disconnection,
    /// reconnects with exponential backoff, entirely on its own
    /// background thread -- never blocking the caller, matching the
    /// user's own explicit requirement) to `lead_addr` (already
    /// `host:port`, e.g. from PlayerSettings::sync_role's own
    /// Follower(lead_addr) combined with sync_publisher_port -- see
    /// mainloop.rs's own update_sync_group), tracking the Lead's own
    /// clock offset from periodic `Sync` messages and yielding
    /// already-offset-corrected `SyncCommand`s via `commands()`.
    pub fn start_follower(lead_addr: String) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (commands_tx, commands_rx) = unbounded::<SyncCommand>();
        let (video_commands_tx, video_commands_rx) = unbounded::<VideoCommand>();
        let reports_addr = reports_address(&lead_addr)?;

        {
            let stop = stop.clone();
            thread::spawn(move || follower_reconnect_loop(lead_addr, commands_tx,
                                                            video_commands_tx, stop));
        }

        Ok(Self { stop, commands: Some(commands_rx), publish: None, current_sync_keys: None,
                  peer_connected: None, video_publish: None,
                  video_commands: Some(video_commands_rx), video_ready: None,
                  reports_addr: Some(reports_addr), peers: None })
    }

    /// (Lead only.) Broadcasts the sync_keys of whichever layout is
    /// now sync-gated to every currently connected Follower -- a no-op
    /// (silently) for a Follower's own SyncGroup, or if the Lead's own
    /// publish thread has somehow already gone away.
    pub fn publish_sync_keys(&self, sync_keys: Vec<String>) {
        if let Some(tx) = &self.publish {
            let _ = tx.send(sync_keys);
        }
    }

    /// (Lead only, no-op for a Follower's own SyncGroup.) Records
    /// which sync_keys this Lead has *actually committed to showing*
    /// right now (empty Vec once it reverts to normal schedule
    /// resolution) -- included in every subsequent periodic `Sync`
    /// heartbeat, letting a (re)connecting Follower catch up on an
    /// already-ongoing Synchronised Event it missed the original
    /// `Command` for (see `current_sync_keys`'s own doc comment for
    /// the full story). The caller (mainloop.rs) is responsible for
    /// only calling this once it has itself actually applied the
    /// switch, never while merely staged/pending -- this method
    /// itself has no way to enforce that.
    pub fn set_current_sync_keys(&self, sync_keys: Vec<String>) {
        if let Some(current) = &self.current_sync_keys {
            *current.lock().expect("poisoned lock") = sync_keys;
        }
    }

    /// (Follower only.) The channel of incoming, already
    /// offset-corrected SyncCommands -- `None` for a Lead's own
    /// SyncGroup.
    pub fn commands(&self) -> Option<&Receiver<SyncCommand>> {
        self.commands.as_ref()
    }

    /// (Lead only.) The channel that fires once per Follower
    /// connection accepted -- `None` for a Follower's own SyncGroup.
    /// See `peer_connected`'s own field doc comment.
    pub fn peer_connected(&self) -> Option<&Receiver<()>> {
        self.peer_connected.as_ref()
    }

    /// (Lead only, silently a no-op for a Follower's own SyncGroup.)
    /// Broadcasts to every connected Follower.
    pub fn publish_video_preload(&self, sync_key: String, round: u32, item: u32, target: String) {
        if let Some(tx) = &self.video_publish {
            let _ = tx.send(VideoPublishRequest::Preload { sync_key, target, item, round });
        }
    }

    pub fn publish_video_play(&self, sync_key: String, round: u32) {
        if let Some(tx) = &self.video_publish {
            let _ = tx.send(VideoPublishRequest::Play { sync_key, round });
        }
    }

    pub fn publish_video_position(&self, sync_key: String, round: u32, position_ms: i64) {
        if let Some(tx) = &self.video_publish {
            let _ = tx.send(VideoPublishRequest::Position { sync_key, position_ms, round });
        }
    }

    /// (Follower only.) Incoming video-wall messages -- `None` for a
    /// Lead's own SyncGroup.
    pub fn video_commands(&self) -> Option<&Receiver<VideoCommand>> {
        self.video_commands.as_ref()
    }

    /// (Lead only.) "Ready to play" reports from any Follower -- `None`
    /// for a Follower's own SyncGroup.
    pub fn video_ready(&self) -> Option<&Receiver<VideoReadyReport>> {
        self.video_ready.as_ref()
    }

    /// (Lead only.) IP addresses of currently connected Followers, from
    /// the same PUB/SUB connections `broadcast` writes to -- lets
    /// mainloop.rs's own orchestration tell how many *distinct*
    /// Followers are actually connected right now, to compare against
    /// how many have reported ready for the current round. Empty (not
    /// `None`) for a Follower's own SyncGroup, and if a stream's own
    /// `peer_addr()` somehow fails (should not happen for a live
    /// connection), that one entry is just skipped rather than erroring
    /// the whole call.
    pub fn connected_follower_addrs(&self) -> Vec<String> {
        let Some(peers) = &self.peers else { return Vec::new() };
        peers.lock().expect("poisoned lock").iter()
            .filter_map(|s| s.peer_addr().ok())
            .map(|a| a.ip().to_string())
            .collect()
    }

    /// (Follower only, silently a no-op for a Lead's own SyncGroup.)
    /// Tells the Lead this Follower has finished buffering `sync_key`
    /// and is ready to play -- a short-lived connection (connect, send
    /// one line, close), since this happens at most once per content
    /// change, not continuously.
    pub fn report_video_ready(&self, sync_key: &str, round: u32) {
        let Some(addr) = &self.reports_addr else { return };
        if let Err(e) = send_video_ready_report(addr, sync_key, round) {
            log::warn!("Sync Group: reporting video ready for {sync_key:?} (round {round}) \
                         to Lead at {addr}: {e:#}");
        }
    }
}

// ---- Lead side ----

fn accept_loop(listener: TcpListener, peers: Arc<Mutex<Vec<TcpStream>>>, stop: Arc<AtomicBool>,
                peer_connected: Sender<()>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => match zmtp_pub_handshake(stream) {
                Ok(stream) => {
                    log::info!("Sync Group: Follower connected from {addr}");
                    peers.lock().expect("poisoned lock").push(stream);
                    // Notify mainloop.rs -- a (re)connecting Follower
                    // (no way to tell a genuinely first-time connection
                    // apart from a reconnection after a restart, on
                    // either side, so this fires unconditionally every
                    // time) needs the currently-active synchronized
                    // layout re-staged and re-published, not just
                    // learned about passively: even once it knows
                    // *which* layout id to show, its own already-
                    // running region/playlist timers would stay
                    // wherever they happened to be if the layout
                    // itself never actually reloads. See mainloop.rs's
                    // own handling of this channel.
                    let _ = peer_connected.send(());
                }
                Err(e) => log::warn!("Sync Group: handshake with Follower at {addr} \
                                       failed: {e:#}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(STOP_CHECK_INTERVAL);
            }
            Err(e) => {
                log::warn!("Sync Group: accept error: {e:#}");
                thread::sleep(STOP_CHECK_INTERVAL);
            }
        }
    }
    log::debug!("Sync Group: Lead accept thread stopped");
}

/// Accepts short-lived connections on the reports port (see
/// REPORTS_PORT_OFFSET), each carrying exactly one newline-delimited
/// JSON `VideoReady` line, then closes -- not ZMTP-framed at all (this
/// is arexibo-to-arexibo only, no XMR-compatibility requirement here).
fn reports_accept_loop(listener: TcpListener, video_ready_tx: Sender<VideoReadyReport>,
                        stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                if let Err(e) = stream.set_read_timeout(Some(StdDuration::from_secs(5))) {
                    log::warn!("Sync Group: setting reports read timeout for {addr}: {e:#}");
                    continue;
                }
                let mut line = String::new();
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(stream);
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {}
                    Ok(_) => match serde_json::from_str::<VideoReady>(line.trim()) {
                        Ok(wire) => {
                            let report = VideoReadyReport {
                                sync_key: wire.sync_key,
                                round: wire.round,
                                follower_addr: addr.ip().to_string(),
                            };
                            let _ = video_ready_tx.send(report);
                        }
                        Err(e) => log::warn!("Sync Group: bad video-ready report from \
                                               {addr}: {e:#}"),
                    },
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(STOP_CHECK_INTERVAL);
            }
            Err(e) => {
                log::warn!("Sync Group: reports accept error: {e:#}");
                thread::sleep(STOP_CHECK_INTERVAL);
            }
        }
    }
    log::debug!("Sync Group: Lead reports accept thread stopped");
}

/// ZMTP handshake for the *server* (PUB) role -- same wire protocol as
/// xmr.rs's own ZmqSubSocket::connect (which does the client/SUB
/// side), mirrored: send our own greeting/READY, validate the peer's.
/// A real SUB peer also sends a `subscribe` command right after its
/// own READY -- read and discard it, since this implementation
/// doesn't do topic-based filtering at all (every message goes to
/// every connected Follower unconditionally; the topic frame still
/// has to be consumed off the wire so it doesn't corrupt the next
/// frame's own framing, even though its content is never inspected).
fn zmtp_pub_handshake(mut stream: TcpStream) -> Result<TcpStream> {
    stream.set_read_timeout(Some(StdDuration::from_secs(5)))
        .context("setting handshake read timeout")?;

    retry_on_eintr(|| stream.write_all(b"\xff\x00\x00\x00\x00\x00\x00\x00\x01\x7f\
                       \x03\x00\
                       NULL\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                       \x00\
                       \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                       \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"))?;
    let mut buf = [0u8; 64];
    retry_on_eintr(|| stream.read_exact(&mut buf))?;
    if buf[0] != 0xff || buf[9] != 0x7f || buf[10] != 0x03 || &buf[12..16] != b"NULL" {
        bail!("ZMTP greeting not understood");
    }

    retry_on_eintr(|| stream.write_all(b"\x04\x19\x05READY\x0bSocket-Type\x00\x00\x00\x03PUB"))?;
    retry_on_eintr(|| stream.read_exact(&mut buf[..2]))?;
    if buf[0] != 0x04 {
        bail!("ZMTP command frame not understood");
    }
    let len = buf[1] as usize;
    if len >= 62 {
        bail!("ZMTP command frame too long");
    }
    retry_on_eintr(|| stream.read_exact(&mut buf[2..2 + len]))?;
    if &buf[2..8] != b"\x05READY" {
        bail!("ZMTP READY command not understood");
    }

    // Consume the SUB peer's own subscribe command (see this
    // function's own doc comment on why it's discarded, not filtered
    // on).
    let flags = retry_on_eintr(|| stream.read_u8())?;
    let long_len = flags & 2 != 0;
    let sub_len = if long_len {
        retry_on_eintr(|| stream.read_u64::<BE>())? as usize
    } else {
        retry_on_eintr(|| stream.read_u8())? as usize
    };
    let mut discard = vec![0u8; sub_len];
    retry_on_eintr(|| stream.read_exact(&mut discard))?;

    Ok(stream)
}

/// Single-frame ZMTP message write -- the PUB-side counterpart of
/// ZmqSubSocket's own recv_frame (which only ever reads). Always uses
/// the long-length (8-byte) encoding for simplicity -- our own JSON
/// messages are at most a couple hundred bytes, so the handful of
/// wasted bytes versus the short-length encoding's own 1-byte form
/// doesn't matter, and this avoids needing a length-dependent branch.
fn send_frame(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut msg = Vec::with_capacity(9 + data.len());
    msg.push(0x02); // flags: not-more, long-length
    msg.write_u64::<BE>(data.len() as u64).expect("Vec write never fails");
    msg.extend_from_slice(data);
    stream.write_all(&msg)
}

fn broadcast(peers: &Arc<Mutex<Vec<TcpStream>>>, msg: &Message) {
    let data = match serde_json::to_vec(msg) {
        Ok(d) => d,
        Err(e) => {
            log::error!("Sync Group: serializing outgoing message: {e:#}");
            return;
        }
    };
    let mut peers = peers.lock().expect("poisoned lock");
    // A write failure means that Follower is gone (disconnected,
    // crashed, network drop) -- drop it from the list rather than
    // trying to detect this separately; it'll reconnect and
    // reappear on its own via the accept loop.
    peers.retain_mut(|stream| send_frame(stream, &data).is_ok());
}

fn lead_publish_loop(peers: Arc<Mutex<Vec<TcpStream>>>, publish_rx: Receiver<Vec<String>>,
                      video_publish_rx: Receiver<VideoPublishRequest>, stop: Arc<AtomicBool>,
                      switch_delay: StdDuration, current_sync_keys: Arc<Mutex<Vec<String>>>) {
    let mut last_sync = Instant::now() - SYNC_INTERVAL; // send one immediately on start
    while !stop.load(Ordering::Relaxed) {
        // A short default timeout doubles as this loop's own `stop`
        // check interval whenever neither channel has anything ready.
        select! {
            recv(publish_rx) -> msg => match msg {
            Ok(sync_keys) => {
                // Broadcasts a Sync right alongside every Command --
                // a real, severe regression found this way: a
                // Follower that receives a Command before it has
                // ever received a Sync message has no real clock
                // offset to correct target_time with. Deferring that
                // Command until a real offset is known (rather than
                // applying it "verbatim", uncorrected) is correct in
                // principle -- but without this, the wait for the
                // *next* periodic Sync (up to SYNC_INTERVAL, 5s away,
                // since the two run on independent schedules) was
                // confirmed real via a live capture to take upwards
                // of 3 seconds -- far worse than the small (~100-
                // 150ms), uncorrected clock-skew error the deferral
                // was meant to fix in the first place. Sending one
                // right here means a Follower's own offset is
                // established (or refreshed) at essentially the same
                // instant it receives this Command, eliminating that
                // multi-second wait entirely. This Sync's own
                // current_sync_keys may be momentarily stale (this
                // Lead's own current_sync_keys only updates once its
                // own apply timer actually fires, same as every other
                // display in the group) -- harmless: the Follower's
                // own catch-up check just compares it against what it
                // already knows, and the Command broadcast right
                // after carries the authoritative new value regardless.
                let current = current_sync_keys.lock().expect("poisoned lock").clone();
                broadcast(&peers, &Message::Sync { lead_time: OffsetDateTime::now_utc(),
                                                    current_sync_keys: current });
                last_sync = Instant::now();

                // `switch_delay` (the CMS's own configured "Switch
                // Delay" for this Sync Group, e.g. 750ms -- previously
                // parsed into PlayerSettings but never actually
                // threaded through to here at all, a real gap found
                // while wiring up the first genuine use of this
                // mechanism) gives every display -- Followers over the
                // network, and this Lead itself -- enough of a lead-in
                // window to receive this Command, apply its own clock-
                // compensation offset, and arm its own local timer
                // *before* the actual target instant arrives. Without
                // it, target_time is already in the past for a
                // Follower by the time the message crosses the network
                // (however small that latency is), so it would just
                // apply as soon as it can rather than at a precisely
                // shared moment -- undermining the whole point of a
                // shared target_time in the first place.
                let target_time = OffsetDateTime::now_utc()
                    + time::Duration::try_from(switch_delay).unwrap_or(time::Duration::ZERO);
                broadcast(&peers, &Message::Command { sync_keys: sync_keys.clone(), target_time });
                log::info!("Sync Group: published Command sync_keys={sync_keys:?} \
                            target_time={target_time}");
            }
                Err(_) => break, // SyncGroup itself was dropped
            },
            recv(video_publish_rx) -> msg => match msg {
                Ok(VideoPublishRequest::Preload { sync_key, target, item, round }) =>
                    broadcast(&peers, &Message::VideoPreload { sync_key, target, item, round }),
                Ok(VideoPublishRequest::Play { sync_key, round }) =>
                    broadcast(&peers, &Message::VideoPlay { sync_key, round }),
                Ok(VideoPublishRequest::Position { sync_key, position_ms, round }) =>
                    broadcast(&peers, &Message::VideoPosition { sync_key, position_ms, round }),
                Err(_) => break, // SyncGroup itself was dropped
            },
            default(StdDuration::from_millis(200)) => {}
        }
        if last_sync.elapsed() >= SYNC_INTERVAL {
            // Read fresh on every heartbeat (not cached/passed in once)
            // -- reflects whatever mainloop.rs's own
            // set_current_sync_keys calls have most recently
            // committed, including reverting to empty once a
            // Synchronised Event's own scheduled window ends (see
            // mainloop.rs's own expiry check).
            let current = current_sync_keys.lock().expect("poisoned lock").clone();
            broadcast(&peers, &Message::Sync { lead_time: OffsetDateTime::now_utc(),
                                                current_sync_keys: current });
            last_sync = Instant::now();
        }
    }
    log::debug!("Sync Group: Lead publish thread stopped");
}

// ---- Follower side ----

/// `lead_addr` is `host:port` -- the reports channel is always
/// `port + REPORTS_PORT_OFFSET` on that same host.
fn reports_address(lead_addr: &str) -> Result<String> {
    let (host, port) = lead_addr.rsplit_once(':')
        .with_context(|| format!("Sync Group Lead address {lead_addr:?} missing a port"))?;
    let port: u16 = port.parse()
        .with_context(|| format!("Sync Group Lead address {lead_addr:?} has an invalid port"))?;
    Ok(format!("{host}:{}", port + REPORTS_PORT_OFFSET))
}

/// Connects, sends one line, closes -- see `report_video_ready`'s own
/// doc comment for why this is a short-lived connection, not a
/// persistent one.
fn send_video_ready_report(addr: &str, sync_key: &str, round: u32) -> Result<()> {
    let mut stream = TcpStream::connect(addr)
        .with_context(|| format!("connecting to Sync Group reports channel at {addr}"))?;
    let mut line = serde_json::to_vec(&VideoReady { sync_key: sync_key.to_string(), round })
        .context("serializing video-ready report")?;
    line.push(b'\n');
    stream.write_all(&line).context("sending video-ready report")?;
    Ok(())
}

fn follower_reconnect_loop(lead_addr: String, commands_tx: Sender<SyncCommand>,
                            video_commands_tx: Sender<VideoCommand>, stop: Arc<AtomicBool>) {
    let mut backoff = StdDuration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        match follower_session(&lead_addr, &commands_tx, &video_commands_tx, &stop) {
            Ok(()) => {
                // Either `stop` was requested (normal, silent exit) or
                // the Lead closed the connection cleanly -- reset
                // backoff either way, so a genuinely brief blip
                // doesn't cause an unnecessarily slow reconnect the
                // next time.
                backoff = StdDuration::from_secs(1);
            }
            Err(e) => {
                log::warn!("Sync Group: Follower connection to Lead at {lead_addr} \
                            failed: {e:#} -- retrying in {backoff:?}");
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut waited = StdDuration::ZERO;
        while waited < backoff && !stop.load(Ordering::Relaxed) {
            let step = STOP_CHECK_INTERVAL.min(backoff - waited);
            thread::sleep(step);
            waited += step;
        }
        backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
    }
    log::debug!("Sync Group: Follower reconnect loop stopped");
}

/// Runs a single connection attempt/session against the Lead --
/// returns `Ok(())` on a clean exit (stop requested, or the Lead
/// closed the connection), `Err` on any failure (connect failure,
/// protocol error, read error), which the caller (the reconnect loop
/// above) turns into a logged warning and a backoff-then-retry.
fn follower_session(lead_addr: &str, commands_tx: &Sender<SyncCommand>,
                     video_commands_tx: &Sender<VideoCommand>,
                     stop: &Arc<AtomicBool>) -> Result<()> {
    // A much shorter read timeout than XMR's own (40s, tuned for XMR's
    // 30s heartbeat cadence) -- Sync Group's own Sync heartbeat is
    // every SYNC_INTERVAL (5s), so this detects a genuinely dead Lead
    // faster, and (just as importantly) means this thread's own `stop`
    // check, tied to this same blocking read, doesn't lag behind a
    // stop request by anywhere near as long either.
    let mut socket = ZmqSubSocket::connect(&format!("tcp://{lead_addr}"), READER_TIMEOUT)
        .context("connecting to Sync Group Lead")?;
    socket.subscribe(b"").context("subscribing to Sync Group Lead")?;
    log::info!("Sync Group: connected to Lead at {lead_addr}");

    // The Lead-vs-local clock offset, refreshed on every Sync message
    // -- None until the first one arrives.
    let mut offset: Option<TimeDuration> = None;
    // A Command received *before* any Sync message (no offset known
    // yet) -- deliberately not applied "verbatim" (a real design flaw
    // found this way: the Lead's own accept_loop can broadcast a
    // fresh Command immediately upon a new Follower connecting, often
    // *before* its own next periodic Sync heartbeat is due, since the
    // two are on independent schedules -- confirmed real via a live
    // capture; applying `target_time` with zero offset correction
    // when the two clocks could genuinely differ is close to useless
    // for the whole point of this mechanism, precise coordination).
    // Held here until the first Sync arrives (see that arm's own
    // handling), then applied then with a *real* offset -- correct,
    // if slightly delayed, rather than immediate but meaningless.
    let mut pending_command: Option<(Vec<String>, OffsetDateTime)> = None;
    // The last sync_keys this Follower knows the Lead to currently
    // have active (whether learned via a real Command, or via a prior
    // catch-up below) -- starts empty on every fresh session
    // (including after a restart), which is exactly what makes the
    // catch-up mechanism below work for a Follower that missed the
    // original Command entirely: its own first Sync heartbeat's own
    // `current_sync_keys` (if non-empty) will always differ from this
    // initial empty state.
    let mut last_known_sync_keys: Vec<String> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        // ZmqSubSocket's own recv_frame has a ~40s internal read
        // timeout (tuned for XMR's own heartbeat cadence, not
        // parameterized) -- `stop` gets checked at least that often,
        // not on every STOP_CHECK_INTERVAL tick like the other loops
        // in this module. Acceptable: reassigning a Follower's own
        // Lead is rare, and this only adds latency to noticing that,
        // not to the actual clock-sync/command precision.
        let (data, _more) = match socket.recv_frame() {
            Ok(f) => f,
            Err(e) => bail!("receiving from Lead: {e:#}"),
        };
        let msg: Message = match serde_json::from_slice(&data) {
            Ok(m) => m,
            Err(_) => continue, // not our own message format -- ignore, not fatal
        };
        match msg {
            Message::Sync { lead_time, current_sync_keys } => {
                offset = Some(lead_time - OffsetDateTime::now_utc());
                // A Command that arrived before we had an offset --
                // apply it now, with a *real* offset this time,
                // before considering the catch-up check below (this
                // naturally keeps last_known_sync_keys correctly in
                // sync, so that check won't then also fire redundantly
                // for the very same sync_keys this pending Command
                // just applied).
                if let Some((sync_keys, target_time)) = pending_command.take() {
                    let target_local = target_time - offset.expect("just set above");
                    log::info!("Sync Group: applying the Command that arrived before \
                                any Sync message, now that a real clock offset is \
                                known (sync_keys {sync_keys:?})");
                    last_known_sync_keys = sync_keys.clone();
                    let _ = commands_tx.send(SyncCommand { sync_keys, target_local });
                }
                // Catch-up mechanism -- a real gap found this way: a
                // Follower that (re)connects mid-way through an
                // already-ongoing Synchronised Event never receives a
                // fresh Command at all (the Lead only publishes one
                // when it *notices a change*; nothing changes from the
                // Lead's own perspective for an already-settled event)
                // -- it would otherwise wait forever. If this
                // heartbeat's own current_sync_keys differs from what
                // this Follower already knows about, synthesize a
                // SyncCommand for it -- applied *immediately*
                // (target_local = now), deliberately not delayed by
                // switch_delay like a genuine fresh Command: this is a
                // one-off correction for a display that's already
                // behind, not a freshly-coordinated simultaneous
                // switch, so there's nothing to wait for.
                if current_sync_keys != last_known_sync_keys {
                    if !current_sync_keys.is_empty() {
                        log::info!("Sync Group: catching up to already-active \
                                    synchronized sync_keys {current_sync_keys:?} via \
                                    Sync heartbeat");
                        let _ = commands_tx.send(SyncCommand {
                            sync_keys: current_sync_keys.clone(),
                            target_local: OffsetDateTime::now_utc(),
                        });
                    }
                    last_known_sync_keys = current_sync_keys;
                }
            }
            Message::Command { sync_keys, target_time } => {
                match offset {
                    Some(off) => {
                        let target_local = target_time - off;
                        last_known_sync_keys = sync_keys.clone();
                        let _ = commands_tx.send(SyncCommand { sync_keys, target_local });
                    }
                    None => {
                        log::info!("Sync Group: received a Command before any Sync \
                                    message -- no real clock offset known yet, \
                                    deferring until the first Sync heartbeat arrives \
                                    (sync_keys {sync_keys:?})");
                        pending_command = Some((sync_keys, target_time));
                    }
                }
            }
            Message::VideoPreload { sync_key, target, item, round } =>
                { let _ = video_commands_tx.send(VideoCommand::Preload { sync_key, target, item, round }); }
            Message::VideoPlay { sync_key, round } =>
                { let _ = video_commands_tx.send(VideoCommand::Play { sync_key, round }); }
            Message::VideoPosition { sync_key, position_ms, round } =>
                { let _ = video_commands_tx.send(VideoCommand::Position { sync_key, position_ms, round }); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_and_follower_exchange_sync_and_command_messages() {
        let leader = SyncGroup::start_lead(0, StdDuration::ZERO).expect("starting lead");
        // start_lead(0) means "any free port" -- but we need the
        // *actual* bound port to connect a follower to it. Rebind
        // deliberately avoided here (0 lets the OS pick, avoiding
        // flaky port collisions between parallel test runs) -- so
        // this test instead binds its own listener first just to
        // learn a genuinely free port, then starts the real Lead on
        // that specific port.
        drop(leader);
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");

        // Wait for the first Sync message to arrive and be processed
        // -- confirmed indirectly: publish a layout and see the
        // resulting Command arrive with a *sensible* target_local
        // (close to "now", not wildly off, which it would be if the
        // offset were never established and defaulted to raw
        // target_time on a machine with a skewed clock -- on the same
        // host in this test, skew is ~0 either way, so this mainly
        // confirms the message round-trips end to end).
        let deadline = Instant::now() + StdDuration::from_secs(10);
        while Instant::now() < deadline {
            leader.publish_sync_keys(vec!["sync1".into()]);
            if let Some(rx) = follower.commands() {
                if let Ok(cmd) = rx.recv_timeout(StdDuration::from_millis(300)) {
                    assert_eq!(cmd.sync_keys, vec!["sync1".to_string()]);
                    let now = OffsetDateTime::now_utc();
                    let drift = (cmd.target_local - now).abs();
                    assert!(drift < TimeDuration::seconds(5),
                            "target_local ({:?}) should be close to now ({now:?}), \
                             got drift {drift:?}", cmd.target_local);
                    return;
                }
            }
        }
        panic!("follower never received a Command within the deadline");
    }

    #[test]
    fn a_command_arrives_quickly_even_right_after_connecting_thanks_to_an_accompanying_sync() {
        // Regression test for a real, *severe* problem found via a
        // live capture, in two stages:
        //
        // 1. The Lead's own accept_loop can broadcast a fresh Command
        //    to a newly-connected Follower *before* its own next
        //    periodic Sync heartbeat is due (the two used to run on
        //    independent schedules) -- confirmed real via "received a
        //    Command before any Sync message" in a live log. Applying
        //    `target_time` "verbatim" (no offset correction) in that
        //    case is close to useless for this mechanism's whole
        //    point, precise coordination.
        //
        // 2. The first fix for that (defer the Command until a real
        //    offset is known, via the next Sync) made things *worse*,
        //    not better: confirmed via a live capture, the wait for
        //    that next periodic Sync (up to SYNC_INTERVAL, 5s, since
        //    the two ran on independent schedules) took upwards of 3
        //    real seconds in one capture -- far worse than the small
        //    (~100-150ms), uncorrected clock-skew error the deferral
        //    was meant to fix in the first place.
        //
        // The real fix (this test's own subject): the Lead now sends
        // a Sync *immediately alongside* every Command it publishes,
        // so a Follower's own offset is established (or refreshed) at
        // essentially the same instant it receives that Command --
        // this test confirms the whole round trip (publish -> receive
        // a usable Command) now completes quickly, not after a
        // multi-second wait.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        // Let the Lead's own *first* periodic Sync (sent immediately
        // on its own publish loop starting, before any Follower has
        // connected at all) fire harmlessly to an empty peer list --
        // its own *next* one won't be due for another ~SYNC_INTERVAL,
        // reliably reproducing the same real window a live Command
        // used to arrive first (unaccompanied) in.
        std::thread::sleep(StdDuration::from_millis(500));

        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");
        // Give the follower's own background thread time to actually
        // finish connecting and subscribing (a classic PUB/SUB "slow
        // joiner" issue -- publishing too soon after start_follower
        // returns risks the Command being broadcast and lost before
        // the subscription has truly taken effect on the wire) --
        // still safely under SYNC_INTERVAL (5s), so the Lead's own
        // next periodic Sync doesn't preempt this test.
        std::thread::sleep(StdDuration::from_secs(1));
        // Published immediately -- mirrors mainloop.rs's own
        // sync_peer_connected handler reacting to this same
        // connection, well before the Lead's own next periodic Sync
        // (not due for several more seconds) would otherwise arrive.
        leader.publish_sync_keys(vec!["sync1".into()]);

        let commands = follower.commands().expect("a Follower's own is always Some");
        // Must arrive quickly -- well under SYNC_INTERVAL (5s) -- since
        // an accompanying Sync is sent right alongside this Command,
        // not several seconds later via the next periodic one.
        let cmd = commands.recv_timeout(StdDuration::from_secs(2))
            .expect("a Command must arrive quickly, thanks to its own accompanying Sync \
                     establishing a real clock offset immediately -- not several seconds \
                     later via the next unrelated periodic Sync");
        assert_eq!(cmd.sync_keys, vec!["sync1".to_string()]);
        let now = OffsetDateTime::now_utc();
        let drift = (cmd.target_local - now).abs();
        assert!(drift < TimeDuration::seconds(2),
                "target_local ({:?}) should be close to now ({now:?}) once a real \
                 offset was applied, got drift {drift:?}", cmd.target_local);
    }

    #[test]
    fn published_target_time_incorporates_the_configured_switch_delay() {
        // Regression test for a real gap: switch_delay (the CMS's own
        // "Switch Delay" setting, parsed into PlayerSettings as
        // sync_switch_delay) used to be parsed but never actually
        // threaded through to the Lead's own publish loop at all --
        // target_time was always essentially "right now", giving
        // Followers no lead-in window to receive the message, apply
        // their own clock offset, and arm a timer before the target
        // instant had already passed.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let switch_delay = StdDuration::from_millis(750);
        let leader = SyncGroup::start_lead(port, switch_delay).expect("starting lead");
        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");

        let deadline = Instant::now() + StdDuration::from_secs(10);
        while Instant::now() < deadline {
            let before = OffsetDateTime::now_utc();
            leader.publish_sync_keys(vec!["sync2".into()]);
            if let Some(rx) = follower.commands() {
                if let Ok(cmd) = rx.recv_timeout(StdDuration::from_millis(300)) {
                    assert_eq!(cmd.sync_keys, vec!["sync2".to_string()]);
                    // target_local should be *ahead* of when we
                    // published by roughly switch_delay (750ms) -- not
                    // just "close to now" (which the other test above
                    // already confirms for the offset-tracking
                    // machinery itself). Generous bounds (500ms-2s)
                    // account for real scheduling jitter in a test
                    // environment, while still clearly distinguishing
                    // "delay applied" from "delay ignored" (which would
                    // put this at ~0ms ahead, well outside this range).
                    let ahead = cmd.target_local - before;
                    assert!(ahead > TimeDuration::milliseconds(500)
                            && ahead < TimeDuration::seconds(2),
                            "target_local should be ~750ms ahead of publish time \
                             (the configured switch_delay), got {ahead:?} instead");
                    return;
                }
            }
        }
        panic!("follower never received a Command within the deadline");
    }

    #[test]
    fn peer_connected_fires_when_a_follower_connects() {
        // Regression coverage for a real report: a Follower that
        // restarted mid-way through an already-active Synchronised
        // Event never got re-synchronized at all -- the Lead only
        // publishes a fresh Command when it *notices a schedule
        // change*, and nothing changes there for an already-settled
        // event. mainloop.rs's own fix reacts to *this* channel
        // firing (re-staging/re-publishing whatever's currently
        // active) -- this test only confirms the channel itself
        // fires on a real connection, which that fix depends on.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        let peer_connected = leader.peer_connected().expect("a Lead's own is always Some");

        assert!(peer_connected.try_recv().is_err(),
                "must not fire before any Follower has connected");

        let _follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");

        peer_connected.recv_timeout(StdDuration::from_secs(5))
            .expect("peer_connected must fire once the Follower actually connects");
    }

    #[test]
    fn a_follower_connecting_after_current_sync_keys_is_set_catches_up_via_sync_heartbeat() {
        // The other half of the same real report: even once told
        // *which* sync_keys are active, a Follower that never received
        // the original Command (because it started, or restarted,
        // after the Lead already committed the switch) needs to learn
        // this from the Lead's own state -- not just from a live
        // Command it necessarily missed. Exercises
        // set_current_sync_keys + the periodic Sync heartbeat's own
        // catch-up logic in follower_session, independently of
        // mainloop.rs's own re-staging reaction to peer_connected.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        // Simulate the Lead having already committed a synchronized
        // switch *before* this Follower ever connects (matching
        // mainloop.rs's own sync_apply_timer handler, which calls
        // this only once actually committed).
        leader.set_current_sync_keys(vec!["sync1".into()]);

        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");

        let cmd = follower.commands().expect("a Follower's own is always Some")
            .recv_timeout(StdDuration::from_secs(10))
            .expect("the Follower must learn about the already-active sync_keys via a \
                     synthesized catch-up SyncCommand, without ever receiving a real Command");
        assert_eq!(cmd.sync_keys, vec!["sync1".to_string()]);
        // Applied immediately (this is a one-off correction for a
        // display that's already behind, not a freshly-coordinated
        // simultaneous switch) -- not delayed by switch_delay.
        let drift = (cmd.target_local - OffsetDateTime::now_utc()).abs();
        assert!(drift < TimeDuration::seconds(2),
                "a catch-up SyncCommand should apply at ~now, got drift {drift:?}");
    }

    #[test]
    fn stopping_frees_the_lead_listen_port_for_reuse() {
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        drop(leader); // should signal the accept thread to stop and release the port

        let deadline = Instant::now() + StdDuration::from_secs(5);
        let mut result = None;
        while Instant::now() < deadline {
            match SyncGroup::start_lead(port, StdDuration::ZERO) {
                Ok(g) => { result = Some(g); break; }
                Err(_) => thread::sleep(StdDuration::from_millis(100)),
            }
        }
        assert!(result.is_some(), "a new Lead should be able to rebind the port \
                                    shortly after the previous one was dropped");
    }

    #[test]
    fn a_follower_with_no_reachable_lead_does_not_block_the_caller() {
        // The whole point of running this on its own background
        // thread -- start_follower must return immediately even
        // though nothing is listening at this address at all.
        let start = Instant::now();
        let follower = SyncGroup::start_follower("127.0.0.1:1".into())
            .expect("starting a follower never fails synchronously");
        assert!(start.elapsed() < StdDuration::from_millis(100),
                "start_follower must not block on the initial connection attempt");
        drop(follower);
    }

    fn free_port() -> u16 {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }

    #[test]
    fn video_preload_play_and_position_reach_the_follower() {
        let port = free_port();
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");
        let rx = follower.video_commands().expect("a follower always has this channel");

        // Retried (not a single send) since the Follower's own ZMTP
        // handshake/subscribe needs a moment to complete after
        // connecting -- a message published before that finishes
        // would never be seen by this specific connection at all.
        let deadline = Instant::now() + StdDuration::from_secs(10);
        let mut got_preload = false;
        while Instant::now() < deadline && !got_preload {
            leader.publish_video_preload("key1".into(), 7, 2, "123.mp4".into());
            if let Ok(VideoCommand::Preload { sync_key, target, item, round }) =
                rx.recv_timeout(StdDuration::from_millis(300)) {
                assert_eq!(sync_key, "key1");
                assert_eq!(target, "123.mp4");
                assert_eq!((item, round), (2, 7));
                got_preload = true;
            }
        }
        assert!(got_preload, "follower never received the Preload command");

        leader.publish_video_play("key1".into(), 7);
        let cmd = rx.recv_timeout(StdDuration::from_secs(2))
            .expect("follower never received the Play command");
        assert_eq!(cmd, VideoCommand::Play { sync_key: "key1".into(), round: 7 });

        leader.publish_video_position("key1".into(), 7, 4200);
        let cmd = rx.recv_timeout(StdDuration::from_secs(2))
            .expect("follower never received the Position command");
        assert_eq!(cmd, VideoCommand::Position { sync_key: "key1".into(), position_ms: 4200, round: 7 });
    }

    #[test]
    fn video_messages_without_round_or_item_still_decode() {
        // Wire format of a version that had neither field.
        let m: Message = serde_json::from_str(
            r#"{"type":"videoPreload","sync_key":"k","target":"a.mp4"}"#).unwrap();
        assert!(matches!(m, Message::VideoPreload { item: 0, round: 0, .. }));
        let m: Message = serde_json::from_str(r#"{"type":"videoPlay","sync_key":"k"}"#).unwrap();
        assert!(matches!(m, Message::VideoPlay { round: 0, .. }));
        let m: Message = serde_json::from_str(
            r#"{"type":"videoPosition","sync_key":"k","position_ms":5}"#).unwrap();
        assert!(matches!(m, Message::VideoPosition { position_ms: 5, round: 0, .. }));
        let r: VideoReady = serde_json::from_str(r#"{"sync_key":"k"}"#).unwrap();
        assert_eq!(r.round, 0);
    }

    #[test]
    fn a_follower_ready_report_reaches_the_lead() {
        let port = free_port();
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");
        let rx = leader.video_ready().expect("a lead always has this channel");

        follower.report_video_ready("key1", 7);
        let report = rx.recv_timeout(StdDuration::from_secs(5))
            .expect("lead never received the ready report");
        assert_eq!(report.sync_key, "key1");
        assert_eq!(report.round, 7);
        // Both ends are 127.0.0.1 in this test, but the field must
        // genuinely be populated (from the reports connection's own
        // peer address), not left blank.
        assert_eq!(report.follower_addr, "127.0.0.1");
    }

    #[test]
    fn connected_follower_addrs_reflects_the_pub_sub_connection_count() {
        let port = free_port();
        let leader = SyncGroup::start_lead(port, StdDuration::ZERO).expect("starting lead");
        assert_eq!(leader.connected_follower_addrs(), Vec::<String>::new(),
                   "no followers connected yet");

        let follower = SyncGroup::start_follower(format!("127.0.0.1:{port}"))
            .expect("starting follower");
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while Instant::now() < deadline && leader.connected_follower_addrs().is_empty() {
            thread::sleep(StdDuration::from_millis(50));
        }
        assert_eq!(leader.connected_follower_addrs(), vec!["127.0.0.1".to_string()]);
        drop(follower);
    }

    #[test]
    fn reports_address_adds_the_offset_to_the_leads_own_port() {
        assert_eq!(reports_address("10.0.0.5:9590").unwrap(), "10.0.0.5:9591");
    }

    #[test]
    fn reports_address_rejects_a_missing_port() {
        assert!(reports_address("10.0.0.5").is_err());
    }
}
