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
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
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

        let stop = Arc::new(AtomicBool::new(false));
        let peers: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let (publish_tx, publish_rx) = unbounded::<Vec<String>>();
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
            thread::spawn(move || lead_publish_loop(peers, publish_rx, stop, switch_delay,
                                                     current_sync_keys));
        }

        log::info!("Sync Group: started as Lead, listening on 0.0.0.0:{port}");
        Ok(Self { stop, commands: None, publish: Some(publish_tx),
                  current_sync_keys: Some(current_sync_keys),
                  peer_connected: Some(peer_connected_rx) })
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

        {
            let stop = stop.clone();
            thread::spawn(move || follower_reconnect_loop(lead_addr, commands_tx, stop));
        }

        Ok(Self { stop, commands: Some(commands_rx), publish: None, current_sync_keys: None,
                  peer_connected: None })
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
                      stop: Arc<AtomicBool>, switch_delay: StdDuration,
                      current_sync_keys: Arc<Mutex<Vec<String>>>) {
    let mut last_sync = Instant::now() - SYNC_INTERVAL; // send one immediately on start
    while !stop.load(Ordering::Relaxed) {
        // A short recv timeout doubles as this loop's own `stop`
        // check interval whenever no layout switch is pending.
        match publish_rx.recv_timeout(StdDuration::from_millis(200)) {
            Ok(sync_keys) => {
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // SyncGroup itself was dropped
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

fn follower_reconnect_loop(lead_addr: String, commands_tx: Sender<SyncCommand>,
                            stop: Arc<AtomicBool>) {
    let mut backoff = StdDuration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        match follower_session(&lead_addr, &commands_tx, &stop) {
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
                let target_local = match offset {
                    Some(off) => target_time - off,
                    None => {
                        log::warn!("Sync Group: received a Command before any Sync \
                                    message -- no clock offset known yet, applying \
                                    target_time verbatim");
                        target_time
                    }
                };
                last_known_sync_keys = sync_keys.clone();
                let _ = commands_tx.send(SyncCommand { sync_keys, target_local });
            }
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
}
