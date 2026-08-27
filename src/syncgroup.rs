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
    },
    Command {
        layout_id: i64,
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
    pub layout_id: i64,
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
    /// Some for a Lead only -- see `publish_layout()`.
    publish: Option<Sender<i64>>,
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
    /// requested via `publish_layout`.
    pub fn start_lead(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))
            .with_context(|| format!("binding Sync Group Lead listener on port {port}"))?;
        listener.set_nonblocking(true)
            .context("setting Sync Group Lead listener non-blocking")?;

        let stop = Arc::new(AtomicBool::new(false));
        let peers: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let (publish_tx, publish_rx) = unbounded::<i64>();

        {
            let stop = stop.clone();
            let peers = peers.clone();
            thread::spawn(move || accept_loop(listener, peers, stop));
        }
        {
            let stop = stop.clone();
            thread::spawn(move || lead_publish_loop(peers, publish_rx, stop));
        }

        log::info!("Sync Group: started as Lead, listening on 0.0.0.0:{port}");
        Ok(Self { stop, commands: None, publish: Some(publish_tx) })
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

        Ok(Self { stop, commands: Some(commands_rx), publish: None })
    }

    /// (Lead only.) Broadcasts a layout switch to every currently
    /// connected Follower -- a no-op (silently) for a Follower's own
    /// SyncGroup, or if the Lead's own publish thread has somehow
    /// already gone away.
    pub fn publish_layout(&self, layout_id: i64) {
        if let Some(tx) = &self.publish {
            let _ = tx.send(layout_id);
        }
    }

    /// (Follower only.) The channel of incoming, already
    /// offset-corrected SyncCommands -- `None` for a Lead's own
    /// SyncGroup.
    pub fn commands(&self) -> Option<&Receiver<SyncCommand>> {
        self.commands.as_ref()
    }
}

// ---- Lead side ----

fn accept_loop(listener: TcpListener, peers: Arc<Mutex<Vec<TcpStream>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => match zmtp_pub_handshake(stream) {
                Ok(stream) => {
                    log::info!("Sync Group: Follower connected from {addr}");
                    peers.lock().expect("poisoned lock").push(stream);
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

fn lead_publish_loop(peers: Arc<Mutex<Vec<TcpStream>>>, publish_rx: Receiver<i64>,
                      stop: Arc<AtomicBool>) {
    let mut last_sync = Instant::now() - SYNC_INTERVAL; // send one immediately on start
    while !stop.load(Ordering::Relaxed) {
        // A short recv timeout doubles as this loop's own `stop`
        // check interval whenever no layout switch is pending.
        match publish_rx.recv_timeout(StdDuration::from_millis(200)) {
            Ok(layout_id) => {
                let target_time = OffsetDateTime::now_utc();
                broadcast(&peers, &Message::Command { layout_id, target_time });
                log::info!("Sync Group: published Command layout={layout_id} \
                            target_time={target_time}");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // SyncGroup itself was dropped
        }
        if last_sync.elapsed() >= SYNC_INTERVAL {
            broadcast(&peers, &Message::Sync { lead_time: OffsetDateTime::now_utc() });
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
            Message::Sync { lead_time } => {
                offset = Some(lead_time - OffsetDateTime::now_utc());
            }
            Message::Command { layout_id, target_time } => {
                let target_local = match offset {
                    Some(off) => target_time - off,
                    None => {
                        log::warn!("Sync Group: received a Command before any Sync \
                                    message -- no clock offset known yet, applying \
                                    target_time verbatim");
                        target_time
                    }
                };
                let _ = commands_tx.send(SyncCommand { layout_id, target_local });
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
        let leader = SyncGroup::start_lead(0).expect("starting lead");
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
        let leader = SyncGroup::start_lead(port).expect("starting lead");
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
            leader.publish_layout(4242);
            if let Some(rx) = follower.commands() {
                if let Ok(cmd) = rx.recv_timeout(StdDuration::from_millis(300)) {
                    assert_eq!(cmd.layout_id, 4242);
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
    fn stopping_frees_the_lead_listen_port_for_reuse() {
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let leader = SyncGroup::start_lead(port).expect("starting lead");
        drop(leader); // should signal the accept thread to stop and release the port

        let deadline = Instant::now() + StdDuration::from_secs(5);
        let mut result = None;
        while Instant::now() < deadline {
            match SyncGroup::start_lead(port) {
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
